//! SQLite persistence for document operations and snapshots.
//!
//! Append-only ops table with periodic snapshots for efficient sync.

use rusqlite::{params, Connection, Result as SqliteResult};
use std::path::Path;

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
    pub position_col: Option<i32>,
    pub position_row: Option<i32>,
    pub parent_document: Option<String>,
    pub created_at: i64,
}

/// Type of document content.
///
/// Role distinctions (User/Model/System) stay at the block level via `Role` enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocumentKind {
    /// Interactive human/model dialog
    Conversation,
    /// Executable code
    Code,
    /// Static markdown/text
    Text,
}

impl DocumentKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            DocumentKind::Conversation => "conversation",
            DocumentKind::Code => "code",
            DocumentKind::Text => "text",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "conversation" => Some(DocumentKind::Conversation),
            "code" => Some(DocumentKind::Code),
            "text" => Some(DocumentKind::Text),
            // Legacy mappings for DB compatibility
            "markdown" => Some(DocumentKind::Text),
            "output" | "system" | "user_message" | "agent_message" => Some(DocumentKind::Conversation),
            _ => None,
        }
    }
}

/// A CRDT operation record.
#[derive(Debug, Clone)]
pub struct OpRecord {
    pub id: i64,
    pub document_id: String,
    pub agent_id: String,
    pub op_bytes: Vec<u8>,
    pub parents: Option<String>, // JSON array
    pub created_at: i64,
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

/// Client sync state for delta sync.
#[derive(Debug, Clone)]
pub struct ClientVersion {
    pub client_id: String,
    pub document_id: String,
    pub last_op_id: i64,
}

const SCHEMA: &str = r#"
-- Operations (append-only, immutable)
CREATE TABLE IF NOT EXISTS ops (
    id INTEGER PRIMARY KEY,
    document_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    op_bytes BLOB NOT NULL,
    parents TEXT,
    created_at INTEGER DEFAULT (unixepoch())
);
CREATE INDEX IF NOT EXISTS idx_ops_document ON ops(document_id, id);

-- Snapshots (periodic materialization)
CREATE TABLE IF NOT EXISTS snapshots (
    document_id TEXT PRIMARY KEY,
    version INTEGER NOT NULL,
    content TEXT NOT NULL,
    oplog_bytes BLOB,
    created_at INTEGER DEFAULT (unixepoch())
);

-- Client sync state
CREATE TABLE IF NOT EXISTS client_versions (
    client_id TEXT NOT NULL,
    document_id TEXT NOT NULL,
    last_op_id INTEGER NOT NULL,
    PRIMARY KEY (client_id, document_id)
);

-- Document metadata
CREATE TABLE IF NOT EXISTS documents (
    id TEXT PRIMARY KEY,
    kind TEXT NOT NULL,
    language TEXT,
    position_col INTEGER,
    position_row INTEGER,
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
            "INSERT INTO documents (id, kind, language, position_col, position_row, parent_document)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                meta.id,
                meta.kind.as_str(),
                meta.language,
                meta.position_col,
                meta.position_row,
                meta.parent_document,
            ],
        )?;
        Ok(())
    }

    /// Get a document by ID.
    pub fn get_document(&self, id: &str) -> SqliteResult<Option<DocumentMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, language, position_col, position_row, parent_document, created_at
             FROM documents WHERE id = ?1",
        )?;

        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            let kind_str: String = row.get(1)?;
            Ok(Some(DocumentMeta {
                id: row.get(0)?,
                kind: DocumentKind::from_str(&kind_str).unwrap_or(DocumentKind::Conversation),
                language: row.get(2)?,
                position_col: row.get(3)?,
                position_row: row.get(4)?,
                parent_document: row.get(5)?,
                created_at: row.get(6)?,
            }))
        } else {
            Ok(None)
        }
    }

    /// List all documents.
    pub fn list_documents(&self) -> SqliteResult<Vec<DocumentMeta>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, kind, language, position_col, position_row, parent_document, created_at
             FROM documents ORDER BY position_row, position_col, created_at",
        )?;

        let rows = stmt.query_map([], |row| {
            let kind_str: String = row.get(1)?;
            Ok(DocumentMeta {
                id: row.get(0)?,
                kind: DocumentKind::from_str(&kind_str).unwrap_or(DocumentKind::Conversation),
                language: row.get(2)?,
                position_col: row.get(3)?,
                position_row: row.get(4)?,
                parent_document: row.get(5)?,
                created_at: row.get(6)?,
            })
        })?;

        rows.collect()
    }

    /// Update document position.
    pub fn update_document_position(&self, id: &str, col: i32, row: i32) -> SqliteResult<()> {
        self.conn.execute(
            "UPDATE documents SET position_col = ?1, position_row = ?2 WHERE id = ?3",
            params![col, row, id],
        )?;
        Ok(())
    }

    /// Delete a document and all its operations.
    pub fn delete_document(&self, id: &str) -> SqliteResult<()> {
        self.conn.execute("DELETE FROM ops WHERE document_id = ?1", params![id])?;
        self.conn.execute("DELETE FROM snapshots WHERE document_id = ?1", params![id])?;
        self.conn.execute("DELETE FROM client_versions WHERE document_id = ?1", params![id])?;
        self.conn.execute("DELETE FROM documents WHERE id = ?1", params![id])?;
        Ok(())
    }

    // =========================================================================
    // Operation log
    // =========================================================================

    /// Append an operation to the log.
    pub fn append_op(
        &self,
        document_id: &str,
        agent_id: &str,
        op_bytes: &[u8],
        parents: Option<&str>,
    ) -> SqliteResult<i64> {
        self.conn.execute(
            "INSERT INTO ops (document_id, agent_id, op_bytes, parents)
             VALUES (?1, ?2, ?3, ?4)",
            params![document_id, agent_id, op_bytes, parents],
        )?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Get operations for a document since a given op ID.
    pub fn get_ops_since(&self, document_id: &str, since_id: i64) -> SqliteResult<Vec<OpRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, document_id, agent_id, op_bytes, parents, created_at
             FROM ops WHERE document_id = ?1 AND id > ?2 ORDER BY id",
        )?;

        let rows = stmt.query_map(params![document_id, since_id], |row| {
            Ok(OpRecord {
                id: row.get(0)?,
                document_id: row.get(1)?,
                agent_id: row.get(2)?,
                op_bytes: row.get(3)?,
                parents: row.get(4)?,
                created_at: row.get(5)?,
            })
        })?;

        rows.collect()
    }

    /// Get all operations for a document.
    pub fn get_all_ops(&self, document_id: &str) -> SqliteResult<Vec<OpRecord>> {
        self.get_ops_since(document_id, 0)
    }

    /// Get the latest op ID for a document.
    pub fn get_latest_op_id(&self, document_id: &str) -> SqliteResult<Option<i64>> {
        self.conn.query_row(
            "SELECT MAX(id) FROM ops WHERE document_id = ?1",
            params![document_id],
            |row| row.get(0),
        )
    }

    /// Count ops for a document since a given ID (for snapshot decision).
    pub fn count_ops_since(&self, document_id: &str, since_id: i64) -> SqliteResult<i64> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM ops WHERE document_id = ?1 AND id > ?2",
            params![document_id, since_id],
            |row| row.get(0),
        )
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
    // Client sync state
    // =========================================================================

    /// Update a client's sync state for a document.
    pub fn update_client_version(
        &self,
        client_id: &str,
        document_id: &str,
        last_op_id: i64,
    ) -> SqliteResult<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO client_versions (client_id, document_id, last_op_id)
             VALUES (?1, ?2, ?3)",
            params![client_id, document_id, last_op_id],
        )?;
        Ok(())
    }

    /// Get a client's sync state for a document.
    pub fn get_client_version(&self, client_id: &str, document_id: &str) -> SqliteResult<Option<i64>> {
        let mut stmt = self.conn.prepare(
            "SELECT last_op_id FROM client_versions WHERE client_id = ?1 AND document_id = ?2",
        )?;

        let mut rows = stmt.query(params![client_id, document_id])?;
        if let Some(row) = rows.next()? {
            Ok(Some(row.get(0)?))
        } else {
            Ok(None)
        }
    }

    /// Get all document versions for a client (for batch sync).
    pub fn get_client_versions(&self, client_id: &str) -> SqliteResult<Vec<ClientVersion>> {
        let mut stmt = self.conn.prepare(
            "SELECT client_id, document_id, last_op_id FROM client_versions WHERE client_id = ?1",
        )?;

        let rows = stmt.query_map(params![client_id], |row| {
            Ok(ClientVersion {
                client_id: row.get(0)?,
                document_id: row.get(1)?,
                last_op_id: row.get(2)?,
            })
        })?;

        rows.collect()
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
            position_col: Some(0),
            position_row: Some(0),
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
    fn test_ops_append_and_query() {
        let db = DocumentDb::in_memory().unwrap();

        let meta = DocumentMeta {
            id: "doc-1".into(),
            kind: DocumentKind::Code,
            language: None,
            position_col: None,
            position_row: None,
            parent_document: None,
            created_at: 0,
        };
        db.create_document(&meta).unwrap();

        // Append some ops
        let id1 = db.append_op("doc-1", "agent-1", b"op1", None).unwrap();
        let id2 = db.append_op("doc-1", "agent-1", b"op2", None).unwrap();
        let id3 = db.append_op("doc-1", "agent-2", b"op3", None).unwrap();

        assert_eq!(id1, 1);
        assert_eq!(id2, 2);
        assert_eq!(id3, 3);

        // Query since
        let ops = db.get_ops_since("doc-1", 1).unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[0].id, 2);
        assert_eq!(ops[1].id, 3);

        // Latest ID
        let latest = db.get_latest_op_id("doc-1").unwrap();
        assert_eq!(latest, Some(3));
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

    #[test]
    fn test_client_versions() {
        let db = DocumentDb::in_memory().unwrap();

        db.update_client_version("client-1", "doc-1", 5).unwrap();
        db.update_client_version("client-1", "doc-2", 10).unwrap();

        let v1 = db.get_client_version("client-1", "doc-1").unwrap();
        assert_eq!(v1, Some(5));

        let versions = db.get_client_versions("client-1").unwrap();
        assert_eq!(versions.len(), 2);

        // Update version
        db.update_client_version("client-1", "doc-1", 15).unwrap();
        let v1 = db.get_client_version("client-1", "doc-1").unwrap();
        assert_eq!(v1, Some(15));
    }
}
