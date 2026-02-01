//! SQLite-backed SSH public key authorization and user identity.
//!
//! Provides:
//! - User management (nick, display_name, admin status)
//! - SSH public key storage and lookup by fingerprint
//! - Import from OpenSSH authorized_keys format

use rusqlite::{params, Connection, Result as SqliteResult};
use russh::keys::ssh_key::{self, HashAlg};
use std::fs;
use std::path::Path;

/// Database handle for authentication.
pub struct AuthDb {
    conn: Connection,
}

/// A kaijutsu user identity.
#[derive(Debug, Clone)]
pub struct User {
    pub id: i64,
    pub nick: String,
    pub display_name: String,
    pub is_admin: bool,
    pub created_at: i64,
}

/// An SSH public key record.
#[derive(Debug, Clone)]
pub struct SshKey {
    pub id: i64,
    pub user_id: i64,
    pub fingerprint: String,
    pub key_type: String,
    pub key_blob: Vec<u8>,
    pub comment: Option<String>,
    pub created_at: i64,
    pub last_used_at: Option<i64>,
}

const SCHEMA: &str = r#"
-- Users (identity + authorization)
CREATE TABLE IF NOT EXISTS users (
    id INTEGER PRIMARY KEY,
    nick TEXT NOT NULL UNIQUE,
    display_name TEXT NOT NULL,
    is_admin INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    created_by INTEGER REFERENCES users(id)
);

-- SSH public keys (authentication)
CREATE TABLE IF NOT EXISTS ssh_keys (
    id INTEGER PRIMARY KEY,
    user_id INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    fingerprint TEXT NOT NULL UNIQUE,
    key_type TEXT NOT NULL,
    key_blob BLOB NOT NULL,
    comment TEXT,
    created_at INTEGER NOT NULL DEFAULT (unixepoch()),
    created_by INTEGER REFERENCES users(id),
    last_used_at INTEGER
);
CREATE INDEX IF NOT EXISTS idx_ssh_keys_fingerprint ON ssh_keys(fingerprint);
"#;

impl AuthDb {
    /// Initialize connection with required PRAGMAs.
    fn init_connection(conn: &Connection) -> SqliteResult<()> {
        // Use execute_batch for PRAGMA statements as they may return results
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;"
        )?;
        Ok(())
    }

    /// Open or create an auth database at the given path.
    pub fn open<P: AsRef<Path>>(path: P) -> SqliteResult<Self> {
        // Ensure parent directory exists
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

    /// Look up a user by SSH key fingerprint.
    ///
    /// Returns the user if the key is authorized, None otherwise.
    pub fn authenticate(&self, fingerprint: &str) -> SqliteResult<Option<User>> {
        let mut stmt = self.conn.prepare(
            "SELECT u.id, u.nick, u.display_name, u.is_admin, u.created_at
             FROM users u
             JOIN ssh_keys k ON k.user_id = u.id
             WHERE k.fingerprint = ?1",
        )?;

        let mut rows = stmt.query(params![fingerprint])?;
        if let Some(row) = rows.next()? {
            Ok(Some(User {
                id: row.get(0)?,
                nick: row.get(1)?,
                display_name: row.get(2)?,
                is_admin: row.get::<_, i64>(3)? != 0,
                created_at: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Update last_used_at for a key (call after successful auth).
    pub fn update_last_used(&self, fingerprint: &str) -> SqliteResult<()> {
        self.conn.execute(
            "UPDATE ssh_keys SET last_used_at = unixepoch() WHERE fingerprint = ?1",
            params![fingerprint],
        )?;
        Ok(())
    }

    // =========================================================================
    // User management
    // =========================================================================

    /// Create a new user.
    pub fn create_user(
        &self,
        nick: &str,
        display_name: &str,
        is_admin: bool,
        created_by: Option<i64>,
    ) -> SqliteResult<i64> {
        self.conn.execute(
            "INSERT INTO users (nick, display_name, is_admin, created_by)
             VALUES (?1, ?2, ?3, ?4)",
            params![nick, display_name, is_admin as i64, created_by],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Get a user by nick.
    pub fn get_user_by_nick(&self, nick: &str) -> SqliteResult<Option<User>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, nick, display_name, is_admin, created_at
             FROM users WHERE nick = ?1",
        )?;

        let mut rows = stmt.query(params![nick])?;
        if let Some(row) = rows.next()? {
            Ok(Some(User {
                id: row.get(0)?,
                nick: row.get(1)?,
                display_name: row.get(2)?,
                is_admin: row.get::<_, i64>(3)? != 0,
                created_at: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// Get a user by ID.
    pub fn get_user(&self, id: i64) -> SqliteResult<Option<User>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, nick, display_name, is_admin, created_at
             FROM users WHERE id = ?1",
        )?;

        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(User {
                id: row.get(0)?,
                nick: row.get(1)?,
                display_name: row.get(2)?,
                is_admin: row.get::<_, i64>(3)? != 0,
                created_at: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// List all users.
    pub fn list_users(&self) -> SqliteResult<Vec<User>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, nick, display_name, is_admin, created_at
             FROM users ORDER BY nick",
        )?;

        let rows = stmt.query_map([], |row| {
            Ok(User {
                id: row.get(0)?,
                nick: row.get(1)?,
                display_name: row.get(2)?,
                is_admin: row.get::<_, i64>(3)? != 0,
                created_at: row.get(4)?,
            })
        })?;

        rows.collect()
    }

    /// Check if the database has any users.
    pub fn is_empty(&self) -> SqliteResult<bool> {
        let count: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0))?;
        Ok(count == 0)
    }

    /// Rename a user (change nick).
    pub fn set_nick(&self, old_nick: &str, new_nick: &str) -> SqliteResult<bool> {
        let updated = self.conn.execute(
            "UPDATE users SET nick = ?1 WHERE nick = ?2",
            params![new_nick, old_nick],
        )?;
        Ok(updated > 0)
    }

    /// Update display name.
    pub fn set_display_name(&self, nick: &str, display_name: &str) -> SqliteResult<bool> {
        let updated = self.conn.execute(
            "UPDATE users SET display_name = ?1 WHERE nick = ?2",
            params![display_name, nick],
        )?;
        Ok(updated > 0)
    }

    /// Remove a user and all their keys (via CASCADE).
    ///
    /// Returns true if a user was deleted, false if not found.
    pub fn remove_user(&self, nick: &str) -> SqliteResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM users WHERE nick = ?1",
            params![nick],
        )?;
        Ok(deleted > 0)
    }

    // =========================================================================
    // SSH key management
    // =========================================================================

    /// Add an SSH key for an existing user.
    pub fn add_key(
        &self,
        user_id: i64,
        key: &ssh_key::PublicKey,
        comment: Option<&str>,
        created_by: Option<i64>,
    ) -> SqliteResult<i64> {
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let key_type = key.algorithm().to_string();
        let key_blob = key.to_bytes().map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e)))
        })?;

        self.conn.execute(
            "INSERT INTO ssh_keys (user_id, fingerprint, key_type, key_blob, comment, created_by)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![user_id, fingerprint, key_type, key_blob, comment, created_by],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Add a key and auto-create a user if needed.
    ///
    /// Nick is derived from the fingerprint tail if not provided.
    /// Display name defaults to the comment or the nick.
    ///
    /// Uses a transaction to ensure atomicity - if key insertion fails,
    /// the user is not created (prevents orphaned users).
    pub fn add_key_auto_user(
        &mut self,
        key: &ssh_key::PublicKey,
        comment: Option<&str>,
        nick: Option<&str>,
        is_admin: bool,
    ) -> SqliteResult<(i64, i64)> {
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();

        // Derive nick from fingerprint if not provided, ensuring uniqueness
        let base_nick = nick
            .map(String::from)
            .unwrap_or_else(|| nick_from_fingerprint(&fingerprint));
        let unique_nick = self.unique_nick(&base_nick)?;

        // Display name from comment or nick
        let display_name = comment.unwrap_or(&unique_nick).to_string();

        // Use transaction to ensure atomicity
        let tx = self.conn.transaction()?;

        let user_id = {
            tx.execute(
                "INSERT INTO users (nick, display_name, is_admin, created_by)
                 VALUES (?1, ?2, ?3, ?4)",
                params![unique_nick, display_name, is_admin as i64, Option::<i64>::None],
            )?;
            tx.last_insert_rowid()
        };

        let key_id = {
            let key_type = key.algorithm().to_string();
            let key_blob = key.to_bytes().map_err(|e| {
                rusqlite::Error::ToSqlConversionFailure(Box::new(std::io::Error::other(e)))
            })?;

            tx.execute(
                "INSERT INTO ssh_keys (user_id, fingerprint, key_type, key_blob, comment, created_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![user_id, fingerprint, key_type, key_blob, comment, Option::<i64>::None],
            )?;
            tx.last_insert_rowid()
        };

        tx.commit()?;
        Ok((user_id, key_id))
    }

    /// Generate a unique nick by appending -1, -2, etc. if needed.
    fn unique_nick(&self, base: &str) -> SqliteResult<String> {
        let mut nick = base.to_string();
        let mut suffix = 1;

        while self.nick_exists(&nick)? {
            nick = format!("{}-{}", base, suffix);
            suffix += 1;
        }

        Ok(nick)
    }

    /// Check if a nick already exists.
    fn nick_exists(&self, nick: &str) -> SqliteResult<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM users WHERE nick = ?1",
            params![nick],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }

    /// List all keys for a user.
    pub fn list_keys(&self, user_id: i64) -> SqliteResult<Vec<SshKey>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, user_id, fingerprint, key_type, key_blob, comment, created_at, last_used_at
             FROM ssh_keys WHERE user_id = ?1 ORDER BY created_at",
        )?;

        let rows = stmt.query_map(params![user_id], |row| {
            Ok(SshKey {
                id: row.get(0)?,
                user_id: row.get(1)?,
                fingerprint: row.get(2)?,
                key_type: row.get(3)?,
                key_blob: row.get(4)?,
                comment: row.get(5)?,
                created_at: row.get(6)?,
                last_used_at: row.get(7)?,
            })
        })?;

        rows.collect()
    }

    /// List all keys in the database.
    pub fn list_all_keys(&self) -> SqliteResult<Vec<(User, SshKey)>> {
        let mut stmt = self.conn.prepare(
            "SELECT u.id, u.nick, u.display_name, u.is_admin, u.created_at,
                    k.id, k.user_id, k.fingerprint, k.key_type, k.key_blob, k.comment, k.created_at, k.last_used_at
             FROM users u
             JOIN ssh_keys k ON k.user_id = u.id
             ORDER BY u.nick, k.created_at",
        )?;

        let rows = stmt.query_map([], |row| {
            let user = User {
                id: row.get(0)?,
                nick: row.get(1)?,
                display_name: row.get(2)?,
                is_admin: row.get::<_, i64>(3)? != 0,
                created_at: row.get(4)?,
            };
            let key = SshKey {
                id: row.get(5)?,
                user_id: row.get(6)?,
                fingerprint: row.get(7)?,
                key_type: row.get(8)?,
                key_blob: row.get(9)?,
                comment: row.get(10)?,
                created_at: row.get(11)?,
                last_used_at: row.get(12)?,
            };
            Ok((user, key))
        })?;

        rows.collect()
    }

    /// Remove a key by fingerprint.
    pub fn remove_key(&self, fingerprint: &str) -> SqliteResult<bool> {
        let deleted = self.conn.execute(
            "DELETE FROM ssh_keys WHERE fingerprint = ?1",
            params![fingerprint],
        )?;
        Ok(deleted > 0)
    }

    /// Get a key by fingerprint.
    pub fn get_key(&self, fingerprint: &str) -> SqliteResult<Option<SshKey>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, user_id, fingerprint, key_type, key_blob, comment, created_at, last_used_at
             FROM ssh_keys WHERE fingerprint = ?1",
        )?;

        let mut rows = stmt.query(params![fingerprint])?;
        if let Some(row) = rows.next()? {
            Ok(Some(SshKey {
                id: row.get(0)?,
                user_id: row.get(1)?,
                fingerprint: row.get(2)?,
                key_type: row.get(3)?,
                key_blob: row.get(4)?,
                comment: row.get(5)?,
                created_at: row.get(6)?,
                last_used_at: row.get(7)?,
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
    /// Creates a user for each unique key. If `first_is_admin` is true,
    /// the first key imported gets admin privileges.
    ///
    /// Returns the number of keys imported.
    pub fn import_authorized_keys<P: AsRef<Path>>(
        &mut self,
        path: P,
        first_is_admin: bool,
    ) -> SqliteResult<usize> {
        let content = fs::read_to_string(path).map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(e))
        })?;

        let mut imported = 0;
        let mut is_first = true;

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

                    let is_admin = first_is_admin && is_first;
                    is_first = false;

                    match self.add_key_auto_user(&key, comment.as_deref(), None, is_admin) {
                        Ok((user_id, _key_id)) => {
                            let nick = self.get_user(user_id)?.map(|u| u.nick).unwrap_or_default();
                            log::info!(
                                "Imported key for user '{}': {} ({})",
                                nick,
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

/// Derive a nick from a fingerprint.
///
/// Takes the last 8 characters of the base64 portion.
/// E.g., "SHA256:abc123...xyz789" → "xyz789" (or similar)
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
        // Generate a test Ed25519 key
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
    fn test_user_crud() {
        let db = AuthDb::in_memory().unwrap();
        assert!(db.is_empty().unwrap());

        let user_id = db.create_user("amy", "Amy Tobey", true, None).unwrap();
        assert!(!db.is_empty().unwrap());

        let user = db.get_user_by_nick("amy").unwrap().unwrap();
        assert_eq!(user.id, user_id);
        assert_eq!(user.nick, "amy");
        assert_eq!(user.display_name, "Amy Tobey");
        assert!(user.is_admin);

        // List users
        let users = db.list_users().unwrap();
        assert_eq!(users.len(), 1);

        // Rename
        assert!(db.set_nick("amy", "atobey").unwrap());
        assert!(db.get_user_by_nick("amy").unwrap().is_none());
        assert!(db.get_user_by_nick("atobey").unwrap().is_some());
    }

    #[test]
    fn test_key_management() {
        let mut db = AuthDb::in_memory().unwrap();
        let key = make_test_key();
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();

        // Add key with auto-user
        let (user_id, key_id) = db
            .add_key_auto_user(&key, Some("test@host"), None, false)
            .unwrap();
        assert!(user_id > 0);
        assert!(key_id > 0);

        // Authenticate
        let user = db.authenticate(&fingerprint).unwrap().unwrap();
        assert_eq!(user.display_name, "test@host");
        assert!(!user.is_admin);

        // List keys
        let keys = db.list_keys(user_id).unwrap();
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
    fn test_add_key_with_custom_nick() {
        let mut db = AuthDb::in_memory().unwrap();
        let key = make_test_key();

        let (user_id, _) = db
            .add_key_auto_user(&key, Some("comment"), Some("custom-nick"), true)
            .unwrap();

        let user = db.get_user(user_id).unwrap().unwrap();
        assert_eq!(user.nick, "custom-nick");
        assert!(user.is_admin);
    }

    #[test]
    fn test_multiple_keys_per_user() {
        let db = AuthDb::in_memory().unwrap();

        let user_id = db.create_user("multikey", "Multi Key User", false, None).unwrap();

        let key1 = make_test_key();
        let key2 = make_test_key();

        db.add_key(user_id, &key1, Some("laptop"), None).unwrap();
        db.add_key(user_id, &key2, Some("desktop"), None).unwrap();

        let keys = db.list_keys(user_id).unwrap();
        assert_eq!(keys.len(), 2);

        // Both keys should authenticate to the same user
        let fp1 = key1.fingerprint(HashAlg::Sha256).to_string();
        let fp2 = key2.fingerprint(HashAlg::Sha256).to_string();

        let user1 = db.authenticate(&fp1).unwrap().unwrap();
        let user2 = db.authenticate(&fp2).unwrap().unwrap();
        assert_eq!(user1.id, user2.id);
    }

    #[test]
    fn test_list_all_keys() {
        let mut db = AuthDb::in_memory().unwrap();

        let key1 = make_test_key();
        let key2 = make_test_key();

        db.add_key_auto_user(&key1, Some("user1@host"), None, true).unwrap();
        db.add_key_auto_user(&key2, Some("user2@host"), None, false).unwrap();

        let all = db.list_all_keys().unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn test_remove_user_cascades_keys() {
        let mut db = AuthDb::in_memory().unwrap();
        let key = make_test_key();
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();

        // Add user with key
        let (user_id, _) = db
            .add_key_auto_user(&key, Some("test@host"), Some("test-user"), false)
            .unwrap();
        assert!(user_id > 0);

        // Verify key exists
        assert!(db.get_key(&fingerprint).unwrap().is_some());

        // Remove user
        assert!(db.remove_user("test-user").unwrap());

        // Key should be gone (CASCADE)
        assert!(db.get_key(&fingerprint).unwrap().is_none());

        // User should be gone
        assert!(db.get_user_by_nick("test-user").unwrap().is_none());
    }

    #[test]
    fn test_nick_collision_handling() {
        let mut db = AuthDb::in_memory().unwrap();

        // Create a user with nick "test"
        db.create_user("test", "Test User 1", false, None).unwrap();

        // Add a key that would derive nick "test" - should get "test-1"
        let key = make_test_key();
        let (user_id, _) = db
            .add_key_auto_user(&key, Some("comment"), Some("test"), false)
            .unwrap();

        let user = db.get_user(user_id).unwrap().unwrap();
        assert_eq!(user.nick, "test-1");

        // Add another - should get "test-2"
        let key2 = make_test_key();
        let (user_id2, _) = db
            .add_key_auto_user(&key2, Some("comment"), Some("test"), false)
            .unwrap();

        let user2 = db.get_user(user_id2).unwrap().unwrap();
        assert_eq!(user2.nick, "test-2");
    }
}
