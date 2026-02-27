//! SQLite persistence for document metadata and snapshots.

use rusqlite::{params, Connection, Result as SqliteResult};
use std::path::Path;
use std::str::FromStr;
use strum::EnumString;

/// Database handle for document persistence.
pub struct DocumentDb {
    conn: Connection,
}

/// Document metadata stored in the database.
#[derive(Debug, Clone)]
pub struct DocumentMeta {
    pub id: String,
    pub kind: DocumentKind,
    pub language: Option<String>,
    pub parent_document: Option<String>,
    pub created_at: i64,
}

/// Type of document content.
///
/// Role distinctions (User/Model/System) stay at the block level via `Role` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString)]
#[strum(ascii_case_insensitive)]
pub enum DocumentKind {
    /// Interactive human/model dialog
    #[strum(serialize = "conversation", serialize = "output", serialize = "system", serialize = "user_message", serialize = "agent_message")]
    Conversation,
    /// Executable code
    Code,
    /// Static markdown/text
    #[strum(serialize = "text", serialize = "markdown")]
    Text,
    /// Configuration file (theme.rhai, models.rhai)
    #[strum(serialize = "config")]
    Config,
}

impl DocumentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            DocumentKind::Conversation => "conversation",
            DocumentKind::Code => "code",
            DocumentKind::Text => "text",
            DocumentKind::Config => "config",
        }
    }

    /// Parse from string (case-insensitive).
    ///
    /// Supports legacy aliases: "markdown" → Text, "output"/"system"/"user_message"/"agent_message" → Conversation.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        <Self as FromStr>::from_str(s).ok()
    }
}

/// A snapshot of a document's content.
#[derive(Debug, Clone)]
pub struct Snapshot {
    pub document_id: String,
    pub version: i64,
    pub content: String,
    pub oplog_bytes: Option<Vec<u8>>,
    pub created_at: i64,
}

const SCHEMA: &str = r#"
-- Snapshots (periodic materialization)
CREATE TABLE IF NOT EXISTS snapshots (
    document_id TEXT PRIMARY KEY,
    version INTEGER NOT NULL,
    content TEXT NOT NULL,
    oplog_bytes BLOB,
    created_at INTEGER DEFAULT (unixepoch())
);

-- Document metadata
CREATE TABLE IF NOT EXISTS documents (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    language TEXT,
    parent_document TEXT,
    created_at INTEGER DEFAULT (unixepoch())
);
"#;

impl DocumentDb {
    /// Open or create a database at the given path.
    pub fn open<P: AsRef<Path>>(path: P) -> SqliteResult<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Create an in-memory database (for testing).
    pub fn in_memory() -> SqliteResult<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    // =========================================================================
    // Document metadata operations
    // =========================================================================

    /// Create a new document.
    pub fn create_document(&self, meta: &DocumentMeta) -> SqliteResult<()> {
        self.conn.execute(
            "INSERT INTO documents (id, kind, language, parent_document)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                meta.id,
                meta.kind.as_str(),
                meta.language,
                meta.parent_document,
            ],
        )?;
        Ok(())
    }

    /// Get a document by ID.
    pub fn get_document(&self, id: &str) -> SqliteResult<Option<DocumentMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, language, parent_document, created_at
             FROM documents WHERE id = ?1",
        )?;

        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            let kind_str: String = row.get(1)?;
            Ok(Some(DocumentMeta {
                id: row.get(0)?,
                kind: DocumentKind::from_str(&kind_str).unwrap_or(DocumentKind::Conversation),
                language: row.get(2)?,
                parent_document: row.get(3)?,
                created_at: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// List all documents.
    pub fn list_documents(&self) -> SqliteResult<Vec<DocumentMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, language, parent_document, created_at
             FROM documents ORDER BY created_at",
        )?;

        let rows = stmt.query_map([], |row| {
            let kind_str: String = row.get(1)?;
            Ok(DocumentMeta {
                id: row.get(0)?,
                kind: DocumentKind::from_str(&kind_str).unwrap_or(DocumentKind::Conversation),
                language: row.get(2)?,
                parent_document: row.get(3)?,
                created_at: row.get(4)?,
            })
        })?;

        rows.collect()
    }

    /// Delete a document and all its snapshots.
    pub fn delete_document(&self, id: &str) -> SqliteResult<()> {
        self.conn.execute("DELETE FROM snapshots WHERE document_id = ?1", params![id])?;
        self.conn.execute("DELETE FROM documents WHERE id = ?1", params![id])?;
        Ok(())
    }

    // =========================================================================
    // Snapshots
    // =========================================================================

    /// Save a snapshot of a document's content.
    pub fn save_snapshot(
        &self,
        document_id: &str,
        version: i64,
        content: &str,
        oplog_bytes: Option<&[u8]>,
    ) -> SqliteResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO snapshots (document_id, version, content, oplog_bytes)
             VALUES (?1, ?2, ?3, ?4)",
            params![document_id, version, content, oplog_bytes],
        )?;
        Ok(())
    }

    /// Get the latest snapshot for a document.
    pub fn get_snapshot(&self, document_id: &str) -> SqliteResult<Option<Snapshot>> {
        let mut stmt = self.conn.prepare(
            "SELECT document_id, version, content, oplog_bytes, created_at
             FROM snapshots WHERE document_id = ?1",
        )?;

        let mut rows = stmt.query(params![document_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(Snapshot {
                document_id: row.get(0)?,
                version: row.get(1)?,
                content: row.get(2)?,
                oplog_bytes: row.get(3)?,
                created_at: row.get(4)?,
            }))
        } else {
            Ok(None)
        }
    }

    // =========================================================================
    // Input document persistence
    // =========================================================================

    /// Ensure the input_docs table exists.
    pub fn ensure_input_docs_table(&self) -> SqliteResult<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS input_docs (
                context_id TEXT PRIMARY KEY,
                content TEXT NOT NULL DEFAULT '',
                oplog_bytes BLOB,
                version INTEGER NOT NULL DEFAULT 0,
                updated_at INTEGER DEFAULT (unixepoch())
            );"
        )
    }

    /// Create or update an input document.
    pub fn upsert_input_doc(
        &self,
        context_id: &str,
        content: &str,
        oplog_bytes: Option<&[u8]>,
        version: i64,
    ) -> SqliteResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO input_docs (context_id, content, oplog_bytes, version, updated_at)
             VALUES (?1, ?2, ?3, ?4, unixepoch())",
            params![context_id, content, oplog_bytes, version],
        )?;
        Ok(())
    }

    /// Create an empty input document (idempotent).
    pub fn create_input_doc(&self, context_id: &str) -> SqliteResult<()> {
        self.conn.execute(
            "INSERT OR IGNORE INTO input_docs (context_id, content, version) VALUES (?1, '', 0)",
            params![context_id],
        )?;
        Ok(())
    }

    /// Clear an input document's content.
    pub fn clear_input_doc(&self, context_id: &str) -> SqliteResult<()> {
        self.conn.execute(
            "UPDATE input_docs SET content = '', oplog_bytes = NULL, version = version + 1, updated_at = unixepoch() WHERE context_id = ?1",
            params![context_id],
        )?;
        Ok(())
    }

    /// Load all input documents.
    pub fn list_input_docs(&self) -> SqliteResult<Vec<(String, Option<Vec<u8>>)>> {
        // Table may not exist yet (DB created before migration)
        let table_exists: bool = self.conn.query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='input_docs'",
            [],
            |row| row.get(0),
        )?;
        if !table_exists {
            return Ok(Vec::new());
        }

        let mut stmt = self.conn.prepare(
            "SELECT context_id, oplog_bytes FROM input_docs"
        )?;

        let rows = stmt.query_map([], |row| {
            let ctx_hex: String = row.get(0)?;
            let oplog_bytes: Option<Vec<u8>> = row.get(1)?;
            Ok((ctx_hex, oplog_bytes))
        })?;

        rows.collect()
    }

    // =========================================================================
    // Bootstrap (stable kernel metadata across restarts)
    // =========================================================================

    /// Ensure the bootstrap key-value table exists.
    pub fn ensure_bootstrap_table(&self) -> SqliteResult<()> {
        self.conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS bootstrap (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL
            );"
        )
    }

    /// Get a bootstrap value by key.
    pub fn get_bootstrap(&self, key: &str) -> SqliteResult<Option<String>> {
        // Table may not exist yet (DB created before migration)
        let table_exists: bool = self.conn.query_row(
            "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type='table' AND name='bootstrap'",
            [],
            |row| row.get(0),
        )?;
        if !table_exists {
            return Ok(None);
        }

        let mut stmt = self.conn.prepare(
            "SELECT value FROM bootstrap WHERE key = ?1"
        )?;
        let mut rows = stmt.query(params![key])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    /// Set a bootstrap value.
    pub fn set_bootstrap(&self, key: &str, value: &str) -> SqliteResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO bootstrap (key, value) VALUES (?1, ?2)",
            params![key, value],
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_document_crud() {
        let db = DocumentDb::in_memory().unwrap();

        let meta = DocumentMeta {
            id: "test-doc-1".into(),
            kind: DocumentKind::Code,
            language: Some("rust".into()),
            parent_document: None,
            created_at: 0,
        };

        db.create_document(&meta).unwrap();

        let loaded = db.get_document("test-doc-1").unwrap().unwrap();
        assert_eq!(loaded.kind, DocumentKind::Code);
        assert_eq!(loaded.language, Some("rust".into()));

        let documents = db.list_documents().unwrap();
        assert_eq!(documents.len(), 1);

        db.delete_document("test-doc-1").unwrap();
        assert!(db.get_document("test-doc-1").unwrap().is_none());
    }

    #[test]
    fn test_snapshots() {
        let db = DocumentDb::in_memory().unwrap();

        db.save_snapshot("doc-1", 10, "hello world", None).unwrap();

        let snap = db.get_snapshot("doc-1").unwrap().unwrap();
        assert_eq!(snap.version, 10);
        assert_eq!(snap.content, "hello world");

        // Update snapshot
        db.save_snapshot("doc-1", 20, "updated content", Some(b"oplog"))
            .unwrap();

        let snap = db.get_snapshot("doc-1").unwrap().unwrap();
        assert_eq!(snap.version, 20);
        assert_eq!(snap.oplog_bytes, Some(b"oplog".to_vec()));
    }
}
