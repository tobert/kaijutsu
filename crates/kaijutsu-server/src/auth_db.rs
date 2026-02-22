//! SQLite-backed SSH public key authorization and principal identity.
//!
//! Provides:
//! - Principal management (username, display_name) backed by PrincipalId (UUIDv7)
//! - SSH public key storage and lookup by fingerprint
//! - Import from OpenSSH authorized_keys format

use kaijutsu_types::{Principal, PrincipalId};
use rusqlite::{params, Connection, Result as SqliteResult};
use russh::keys::ssh_key::{self, HashAlg};
use std::fs;
use std::path::Path;

/// Database handle for authentication.
pub struct AuthDb {
    conn: Connection,
}

/// An SSH public key record (DB columns not on Credential).
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

impl AuthDb {
    /// Initialize connection with required PRAGMAs.
    fn init_connection(conn: &Connection) -> SqliteResult<()> {
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;",
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
        Ok(Self { conn })
    }

    /// Create an in-memory database (for testing).
    pub fn in_memory() -> SqliteResult<Self> {
        let conn = Connection::open_in_memory()?;
        Self::init_connection(&conn)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
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
    /// Returns the principal if the key is authorized, None otherwise.
    pub fn authenticate(&self, fingerprint: &str) -> SqliteResult<Option<Principal>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.username, p.display_name
             FROM principals p
             JOIN credentials c ON c.principal_id = p.id
             WHERE c.fingerprint = ?1",
        )?;

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
            Ok(Some(Principal {
                id,
                username: row.get(1)?,
                display_name: row.get(2)?,
            }))
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
    // Principal management
    // =========================================================================

    /// Create a new principal. Generates a fresh PrincipalId.
    pub fn create_principal(
        &self,
        username: &str,
        display_name: &str,
    ) -> SqliteResult<PrincipalId> {
        let id = PrincipalId::new();
        self.conn.execute(
            "INSERT INTO principals (id, username, display_name)
             VALUES (?1, ?2, ?3)",
            params![id.as_bytes().as_slice(), username, display_name],
        )?;
        Ok(id)
    }

    /// Get a principal by username.
    pub fn get_principal_by_username(
        &self,
        username: &str,
    ) -> SqliteResult<Option<Principal>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, username, display_name
             FROM principals WHERE username = ?1",
        )?;

        let mut rows = stmt.query(params![username])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_principal(row)?))
        } else {
            Ok(None)
        }
    }

    /// Get a principal by ID.
    pub fn get_principal(&self, id: PrincipalId) -> SqliteResult<Option<Principal>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, username, display_name
             FROM principals WHERE id = ?1",
        )?;

        let mut rows = stmt.query(params![id.as_bytes().as_slice()])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row_to_principal(row)?))
        } else {
            Ok(None)
        }
    }

    /// List all principals.
    pub fn list_principals(&self) -> SqliteResult<Vec<Principal>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, username, display_name
             FROM principals ORDER BY username",
        )?;

        let rows = stmt.query_map([], |row| row_to_principal(row))?;
        rows.collect()
    }

    /// Check if the database has any principals.
    pub fn is_empty(&self) -> SqliteResult<bool> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM principals", [], |row| row.get(0))?;
        Ok(count == 0)
    }

    /// Rename a principal (change username).
    pub fn set_username(&self, old_username: &str, new_username: &str) -> SqliteResult<bool> {
        let updated = self.conn.execute(
            "UPDATE principals SET username = ?1 WHERE username = ?2",
            params![new_username, old_username],
        )?;
        Ok(updated > 0)
    }

    /// Update display name.
    pub fn set_display_name(&self, username: &str, display_name: &str) -> SqliteResult<bool> {
        let updated = self.conn.execute(
            "UPDATE principals SET display_name = ?1 WHERE username = ?2",
            params![display_name, username],
        )?;
        Ok(updated > 0)
    }

    /// Remove a principal and all their keys (via CASCADE).
    pub fn remove_principal(&self, username: &str) -> SqliteResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM principals WHERE username = ?1",
            params![username],
        )?;
        Ok(deleted > 0)
    }

    // =========================================================================
    // SSH key management
    // =========================================================================

    /// Add an SSH key for an existing principal.
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

    /// Add a key and auto-create a principal if needed.
    ///
    /// Username is derived from the fingerprint tail if not provided.
    /// Display name defaults to the comment or the username.
    ///
    /// Returns (PrincipalId, fingerprint).
    pub fn add_key_auto_principal(
        &mut self,
        key: &ssh_key::PublicKey,
        comment: Option<&str>,
        username: Option<&str>,
    ) -> SqliteResult<(PrincipalId, String)> {
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();

        // Derive username from fingerprint if not provided
        let base_username = username
            .map(String::from)
            .unwrap_or_else(|| nick_from_fingerprint(&fingerprint));
        let unique_username = self.unique_username(&base_username)?;

        // Display name from comment or username
        let display_name = comment.unwrap_or(&unique_username).to_string();

        // Use transaction to ensure atomicity
        let tx = self.conn.transaction()?;

        let principal_id = PrincipalId::new();

        tx.execute(
            "INSERT INTO principals (id, username, display_name)
             VALUES (?1, ?2, ?3)",
            params![
                principal_id.as_bytes().as_slice(),
                unique_username,
                display_name
            ],
        )?;

        let key_type = key.algorithm().to_string();
        let key_blob = key.to_bytes().map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e)))
        })?;

        tx.execute(
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

        tx.commit()?;
        Ok((principal_id, fingerprint))
    }

    /// Generate a unique username by appending -1, -2, etc. if needed.
    fn unique_username(&self, base: &str) -> SqliteResult<String> {
        let mut username = base.to_string();
        let mut suffix = 1;

        while self.username_exists(&username)? {
            username = format!("{}-{}", base, suffix);
            suffix += 1;
        }

        Ok(username)
    }

    /// Check if a username already exists.
    fn username_exists(&self, username: &str) -> SqliteResult<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM principals WHERE username = ?1",
            params![username],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// List all keys for a principal.
    pub fn list_keys(&self, principal_id: PrincipalId) -> SqliteResult<Vec<SshKeyRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT fingerprint, principal_id, key_type, key_blob, comment, created_at, last_used_at
             FROM credentials WHERE principal_id = ?1 ORDER BY created_at",
        )?;

        let rows = stmt.query_map(params![principal_id.as_bytes().as_slice()], |row| {
            let pid_bytes: Vec<u8> = row.get(1)?;
            let pid = PrincipalId::try_from_slice(&pid_bytes).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Blob,
                    "invalid PrincipalId".into(),
                )
            })?;
            Ok(SshKeyRecord {
                fingerprint: row.get(0)?,
                principal_id: pid,
                key_type: row.get(2)?,
                key_blob: row.get(3)?,
                comment: row.get(4)?,
                created_at: row.get(5)?,
                last_used_at: row.get(6)?,
            })
        })?;

        rows.collect()
    }

    /// List all keys in the database.
    pub fn list_all_keys(&self) -> SqliteResult<Vec<(Principal, SshKeyRecord)>> {
        let mut stmt = self.conn.prepare(
            "SELECT p.id, p.username, p.display_name,
                    c.fingerprint, c.principal_id, c.key_type, c.key_blob, c.comment, c.created_at, c.last_used_at
             FROM principals p
             JOIN credentials c ON c.principal_id = p.id
             ORDER BY p.username, c.created_at",
        )?;

        let rows = stmt.query_map([], |row| {
            let principal = row_to_principal(row)?;
            let pid_bytes: Vec<u8> = row.get(4)?;
            let pid = PrincipalId::try_from_slice(&pid_bytes).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    4,
                    rusqlite::types::Type::Blob,
                    "invalid PrincipalId".into(),
                )
            })?;
            let key = SshKeyRecord {
                fingerprint: row.get(3)?,
                principal_id: pid,
                key_type: row.get(5)?,
                key_blob: row.get(6)?,
                comment: row.get(7)?,
                created_at: row.get(8)?,
                last_used_at: row.get(9)?,
            };
            Ok((principal, key))
        })?;

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

    /// Get a key by fingerprint.
    pub fn get_key(&self, fingerprint: &str) -> SqliteResult<Option<SshKeyRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT fingerprint, principal_id, key_type, key_blob, comment, created_at, last_used_at
             FROM credentials WHERE fingerprint = ?1",
        )?;

        let mut rows = stmt.query(params![fingerprint])?;
        if let Some(row) = rows.next()? {
            let pid_bytes: Vec<u8> = row.get(1)?;
            let pid = PrincipalId::try_from_slice(&pid_bytes).ok_or_else(|| {
                rusqlite::Error::FromSqlConversionFailure(
                    1,
                    rusqlite::types::Type::Blob,
                    "invalid PrincipalId".into(),
                )
            })?;
            Ok(Some(SshKeyRecord {
                fingerprint: row.get(0)?,
                principal_id: pid,
                key_type: row.get(2)?,
                key_blob: row.get(3)?,
                comment: row.get(4)?,
                created_at: row.get(5)?,
                last_used_at: row.get(6)?,
            }))
        } else {
            Ok(None)
        }
    }

    // =========================================================================
    // Import
    // =========================================================================

    /// Import keys from an OpenSSH authorized_keys file.
    ///
    /// Creates a principal for each unique key.
    ///
    /// Returns the number of keys imported.
    pub fn import_authorized_keys<P: AsRef<Path>>(
        &mut self,
        path: P,
    ) -> SqliteResult<usize> {
        let content = fs::read_to_string(path).map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(e))
        })?;

        let mut imported = 0;

        for line in content.lines() {
            let line = line.trim();

            // Skip empty lines and comments
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            // Parse the key
            match ssh_key::PublicKey::from_openssh(line) {
                Ok(key) => {
                    let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();

                    // Skip if key already exists
                    if self.get_key(&fingerprint)?.is_some() {
                        log::debug!("Skipping existing key: {}", fingerprint);
                        continue;
                    }

                    // Extract comment (last field after key data)
                    let comment = extract_comment(line);

                    match self.add_key_auto_principal(&key, comment.as_deref(), None) {
                        Ok((principal_id, _fingerprint)) => {
                            let username = self
                                .get_principal(principal_id)?
                                .map(|p| p.username)
                                .unwrap_or_default();
                            log::info!(
                                "Imported key for '{}': {} ({})",
                                username,
                                fingerprint,
                                comment.as_deref().unwrap_or("no comment")
                            );
                            imported += 1;
                        }
                        Err(e) => {
                            log::warn!("Failed to import key {}: {}", fingerprint, e);
                        }
                    }
                }
                Err(e) => {
                    log::warn!("Failed to parse key line: {} ({})", e, line);
                }
            }
        }

        Ok(imported)
    }
}

/// Extract a Principal from a row with columns (id, username, display_name).
fn row_to_principal(row: &rusqlite::Row<'_>) -> SqliteResult<Principal> {
    let id_bytes: Vec<u8> = row.get(0)?;
    let id = PrincipalId::try_from_slice(&id_bytes).ok_or_else(|| {
        rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Blob,
            "invalid PrincipalId bytes".into(),
        )
    })?;
    Ok(Principal {
        id,
        username: row.get(1)?,
        display_name: row.get(2)?,
    })
}

/// Derive a username from a fingerprint.
///
/// Takes the last 8 characters of the base64 portion.
fn nick_from_fingerprint(fingerprint: &str) -> String {
    fingerprint
        .trim_start_matches("SHA256:")
        .chars()
        .rev()
        .take(8)
        .collect::<String>()
        .chars()
        .rev()
        .collect()
}

/// Extract comment from an authorized_keys line.
///
/// Format: "key-type base64-data [comment]"
fn extract_comment(line: &str) -> Option<String> {
    let parts: Vec<&str> = line.splitn(3, ' ').collect();
    if parts.len() >= 3 {
        Some(parts[2].to_string())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_test_key() -> ssh_key::PublicKey {
        let private = russh::keys::PrivateKey::random(
            &mut rand::thread_rng(),
            russh::keys::Algorithm::Ed25519,
        )
        .unwrap();
        private.public_key().clone()
    }

    #[test]
    fn test_nick_from_fingerprint() {
        assert_eq!(
            nick_from_fingerprint("SHA256:abcdefghijklmnop"),
            "ijklmnop"
        );
        assert_eq!(nick_from_fingerprint("SHA256:short"), "short");
        assert_eq!(nick_from_fingerprint("SHA256:12345678"), "12345678");
    }

    #[test]
    fn test_extract_comment() {
        assert_eq!(
            extract_comment("ssh-ed25519 AAAA... amy@laptop"),
            Some("amy@laptop".to_string())
        );
        assert_eq!(
            extract_comment("ssh-ed25519 AAAA... user@host with spaces"),
            Some("user@host with spaces".to_string())
        );
        assert_eq!(extract_comment("ssh-ed25519 AAAA..."), None);
    }

    #[test]
    fn test_principal_crud() {
        let db = AuthDb::in_memory().unwrap();
        assert!(db.is_empty().unwrap());

        let id = db
            .create_principal("amy", "Amy Tobey")
            .unwrap();
        assert!(!db.is_empty().unwrap());

        let principal = db.get_principal_by_username("amy").unwrap().unwrap();
        assert_eq!(principal.id, id);
        assert_eq!(principal.username, "amy");
        assert_eq!(principal.display_name, "Amy Tobey");

        // List principals
        let principals = db.list_principals().unwrap();
        assert_eq!(principals.len(), 1);

        // Rename
        assert!(db.set_username("amy", "atobey").unwrap());
        assert!(db
            .get_principal_by_username("amy")
            .unwrap()
            .is_none());
        assert!(db
            .get_principal_by_username("atobey")
            .unwrap()
            .is_some());
    }

    #[test]
    fn test_key_management() {
        let mut db = AuthDb::in_memory().unwrap();
        let key = make_test_key();
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();

        // Add key with auto-principal
        let (principal_id, fp) = db
            .add_key_auto_principal(&key, Some("test@host"), None)
            .unwrap();
        assert!(!principal_id.is_nil());
        assert_eq!(fp, fingerprint);

        // Authenticate
        let principal = db.authenticate(&fingerprint).unwrap().unwrap();
        assert_eq!(principal.display_name, "test@host");

        // List keys
        let keys = db.list_keys(principal_id).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].fingerprint, fingerprint);

        // Update last used
        db.update_last_used(&fingerprint).unwrap();
        let key_record = db.get_key(&fingerprint).unwrap().unwrap();
        assert!(key_record.last_used_at.is_some());

        // Remove key
        assert!(db.remove_key(&fingerprint).unwrap());
        assert!(db.authenticate(&fingerprint).unwrap().is_none());
    }

    #[test]
    fn test_add_key_with_custom_username() {
        let mut db = AuthDb::in_memory().unwrap();
        let key = make_test_key();

        let (principal_id, _) = db
            .add_key_auto_principal(&key, Some("comment"), Some("custom-nick"))
            .unwrap();

        let principal = db.get_principal(principal_id).unwrap().unwrap();
        assert_eq!(principal.username, "custom-nick");
    }

    #[test]
    fn test_multiple_keys_per_principal() {
        let db = AuthDb::in_memory().unwrap();

        let principal_id = db
            .create_principal("multikey", "Multi Key User")
            .unwrap();

        let key1 = make_test_key();
        let key2 = make_test_key();

        db.add_key(principal_id, &key1, Some("laptop")).unwrap();
        db.add_key(principal_id, &key2, Some("desktop")).unwrap();

        let keys = db.list_keys(principal_id).unwrap();
        assert_eq!(keys.len(), 2);

        // Both keys should authenticate to the same principal
        let fp1 = key1.fingerprint(HashAlg::Sha256).to_string();
        let fp2 = key2.fingerprint(HashAlg::Sha256).to_string();

        let p1 = db.authenticate(&fp1).unwrap().unwrap();
        let p2 = db.authenticate(&fp2).unwrap().unwrap();
        assert_eq!(p1.id, p2.id);
    }

    #[test]
    fn test_list_all_keys() {
        let mut db = AuthDb::in_memory().unwrap();

        let key1 = make_test_key();
        let key2 = make_test_key();

        db.add_key_auto_principal(&key1, Some("user1@host"), None)
            .unwrap();
        db.add_key_auto_principal(&key2, Some("user2@host"), None)
            .unwrap();

        let all = db.list_all_keys().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_remove_principal_cascades_keys() {
        let mut db = AuthDb::in_memory().unwrap();
        let key = make_test_key();
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();

        // Add principal with key
        let (principal_id, _) = db
            .add_key_auto_principal(&key, Some("test@host"), Some("test-user"))
            .unwrap();
        assert!(!principal_id.is_nil());

        // Verify key exists
        assert!(db.get_key(&fingerprint).unwrap().is_some());

        // Remove principal
        assert!(db.remove_principal("test-user").unwrap());

        // Key should be gone (CASCADE)
        assert!(db.get_key(&fingerprint).unwrap().is_none());

        // Principal should be gone
        assert!(db
            .get_principal_by_username("test-user")
            .unwrap()
            .is_none());
    }

    #[test]
    fn test_username_collision_handling() {
        let mut db = AuthDb::in_memory().unwrap();

        // Create a principal with username "test"
        db.create_principal("test", "Test User 1").unwrap();

        // Add a key that would derive username "test" - should get "test-1"
        let key = make_test_key();
        let (principal_id, _) = db
            .add_key_auto_principal(&key, Some("comment"), Some("test"))
            .unwrap();

        let principal = db.get_principal(principal_id).unwrap().unwrap();
        assert_eq!(principal.username, "test-1");

        // Add another - should get "test-2"
        let key2 = make_test_key();
        let (principal_id2, _) = db
            .add_key_auto_principal(&key2, Some("comment"), Some("test"))
            .unwrap();

        let principal2 = db.get_principal(principal_id2).unwrap().unwrap();
        assert_eq!(principal2.username, "test-2");
    }

    #[test]
    fn test_principal_id_roundtrip() {
        let db = AuthDb::in_memory().unwrap();
        let id = db.create_principal("roundtrip", "Roundtrip Test").unwrap();
        let principal = db.get_principal(id).unwrap().unwrap();
        assert_eq!(principal.id, id);
        assert_eq!(principal.username, "roundtrip");
    }
}
