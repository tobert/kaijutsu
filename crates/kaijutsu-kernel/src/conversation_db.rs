//! SQLite persistence for conversations.
//!
//! Stores conversation metadata, participants, mounts, and the serialized BlockDocument snapshot.

use rusqlite::{params, Connection, Result as SqliteResult};
use std::path::Path;

use crate::conversation::{AccessLevel, Conversation, Mount, Participant, ParticipantKind};
use kaijutsu_crdt::{BlockDocument, DocumentSnapshot};

/// Database handle for conversation persistence.
pub struct ConversationDb {
    conn: Connection,
}

const SCHEMA: &str = r#"
-- Conversation metadata
CREATE TABLE IF NOT EXISTS conversations (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    doc_encoded BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_conversations_updated ON conversations(updated_at DESC);

-- Conversation participants
CREATE TABLE IF NOT EXISTS participants (
    conversation_id TEXT NOT NULL,
    participant_id TEXT NOT NULL,
    display_name TEXT NOT NULL,
    kind TEXT NOT NULL,
    provider TEXT,
    model_id TEXT,
    joined_at INTEGER NOT NULL,
    PRIMARY KEY (conversation_id, participant_id),
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

-- Conversation mounts
CREATE TABLE IF NOT EXISTS mounts (
    conversation_id TEXT NOT NULL,
    mount_path TEXT NOT NULL,
    kernel_id TEXT NOT NULL,
    access TEXT NOT NULL,
    PRIMARY KEY (conversation_id, mount_path),
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);
"#;

impl ConversationDb {
    /// Open or create a database at the given path.
    pub fn open<P: AsRef<Path>>(path: P) -> SqliteResult<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    /// Create an in-memory database (for testing).
    pub fn in_memory() -> SqliteResult<Self> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch("PRAGMA foreign_keys = ON;")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    // =========================================================================
    // Conversation CRUD
    // =========================================================================

    /// Save a conversation (insert or replace).
    pub fn save(&self, conv: &Conversation) -> SqliteResult<()> {
        // Serialize the document snapshot as JSON
        let snapshot = conv.doc.snapshot();
        let doc_encoded = serde_json::to_vec(&snapshot).map_err(|e| {
            rusqlite::Error::ToSqlConversionFailure(Box::new(e))
        })?;

        // Use a transaction for atomicity
        let tx = self.conn.unchecked_transaction()?;

        // Upsert conversation metadata
        tx.execute(
            "INSERT OR REPLACE INTO conversations (id, name, doc_encoded, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                conv.id,
                conv.name,
                doc_encoded,
                conv.created_at as i64,
                conv.updated_at as i64,
            ],
        )?;

        // Delete existing participants and mounts (we'll reinsert)
        tx.execute(
            "DELETE FROM participants WHERE conversation_id = ?1",
            params![conv.id],
        )?;
        tx.execute(
            "DELETE FROM mounts WHERE conversation_id = ?1",
            params![conv.id],
        )?;

        // Insert participants
        for p in &conv.participants {
            let (kind_str, provider, model_id) = match &p.kind {
                ParticipantKind::User => ("user", None, None),
                ParticipantKind::Model { provider, model_id } => {
                    ("model", Some(provider.as_str()), Some(model_id.as_str()))
                }
            };
            tx.execute(
                "INSERT INTO participants (conversation_id, participant_id, display_name, kind, provider, model_id, joined_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![
                    conv.id,
                    p.id,
                    p.display_name,
                    kind_str,
                    provider,
                    model_id,
                    p.joined_at as i64,
                ],
            )?;
        }

        // Insert mounts
        for m in &conv.mounts {
            let access_str = match m.access {
                AccessLevel::Read => "read",
                AccessLevel::ReadWrite => "read_write",
            };
            tx.execute(
                "INSERT INTO mounts (conversation_id, mount_path, kernel_id, access)
                 VALUES (?1, ?2, ?3, ?4)",
                params![conv.id, m.mount_path, m.kernel_id, access_str],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    /// Load a conversation by ID.
    pub fn load(&self, id: &str) -> SqliteResult<Option<Conversation>> {
        // Load conversation metadata
        let mut stmt = self.conn.prepare(
            "SELECT id, name, doc_encoded, created_at, updated_at
             FROM conversations WHERE id = ?1",
        )?;

        let mut rows = stmt.query(params![id])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };

        let conv_id: String = row.get(0)?;
        let name: String = row.get(1)?;
        let doc_encoded: Vec<u8> = row.get(2)?;
        let created_at: i64 = row.get(3)?;
        let updated_at: i64 = row.get(4)?;

        // Deserialize the BlockDocument from snapshot
        let snapshot: DocumentSnapshot = serde_json::from_slice(&doc_encoded).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(
                2,
                rusqlite::types::Type::Blob,
                Box::new(e),
            )
        })?;
        let doc = BlockDocument::from_snapshot(snapshot, "server");

        // Load participants
        let participants = self.load_participants(&conv_id)?;

        // Load mounts
        let mounts = self.load_mounts(&conv_id)?;

        Ok(Some(Conversation {
            id: conv_id,
            name,
            doc,
            participants,
            mounts,
            created_at: created_at as u64,
            updated_at: updated_at as u64,
        }))
    }

    /// Load participants for a conversation.
    fn load_participants(&self, conversation_id: &str) -> SqliteResult<Vec<Participant>> {
        let mut stmt = self.conn.prepare(
            "SELECT participant_id, display_name, kind, provider, model_id, joined_at
             FROM participants WHERE conversation_id = ?1",
        )?;

        let rows = stmt.query_map(params![conversation_id], |row| {
            let id: String = row.get(0)?;
            let display_name: String = row.get(1)?;
            let kind_str: String = row.get(2)?;
            let provider: Option<String> = row.get(3)?;
            let model_id: Option<String> = row.get(4)?;
            let joined_at: i64 = row.get(5)?;

            let kind = match kind_str.as_str() {
                "user" => ParticipantKind::User,
                "model" => ParticipantKind::Model {
                    provider: provider.unwrap_or_default(),
                    model_id: model_id.unwrap_or_default(),
                },
                _ => ParticipantKind::User,
            };

            Ok(Participant {
                id,
                display_name,
                kind,
                joined_at: joined_at as u64,
            })
        })?;

        rows.collect()
    }

    /// Load mounts for a conversation.
    fn load_mounts(&self, conversation_id: &str) -> SqliteResult<Vec<Mount>> {
        let mut stmt = self.conn.prepare(
            "SELECT mount_path, kernel_id, access FROM mounts WHERE conversation_id = ?1",
        )?;

        let rows = stmt.query_map(params![conversation_id], |row| {
            let mount_path: String = row.get(0)?;
            let kernel_id: String = row.get(1)?;
            let access_str: String = row.get(2)?;

            let access = match access_str.as_str() {
                "read_write" => AccessLevel::ReadWrite,
                _ => AccessLevel::Read,
            };

            Ok(Mount {
                kernel_id,
                mount_path,
                access,
            })
        })?;

        rows.collect()
    }

    /// Load all conversations (ordered by updated_at descending).
    pub fn load_all(&self) -> SqliteResult<Vec<Conversation>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM conversations ORDER BY updated_at DESC",
        )?;

        let ids: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        let mut conversations = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(conv) = self.load(&id)? {
                conversations.push(conv);
            }
        }

        Ok(conversations)
    }

    /// Delete a conversation.
    pub fn delete(&self, id: &str) -> SqliteResult<()> {
        // Foreign key cascade handles participants and mounts
        self.conn.execute(
            "DELETE FROM conversations WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// List conversation IDs (without loading full content).
    pub fn list_ids(&self) -> SqliteResult<Vec<String>> {
        let mut stmt = self.conn.prepare(
            "SELECT id FROM conversations ORDER BY updated_at DESC",
        )?;

        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect()
    }

    /// Get conversation count.
    pub fn count(&self) -> SqliteResult<usize> {
        self.conn.query_row(
            "SELECT COUNT(*) FROM conversations",
            [],
            |row| row.get::<_, i64>(0),
        ).map(|c| c as usize)
    }

    /// Check if a conversation exists.
    pub fn exists(&self, id: &str) -> SqliteResult<bool> {
        let count: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM conversations WHERE id = ?1",
            params![id],
            |row| row.get(0),
        )?;
        Ok(count > 0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_save_and_load() {
        let db = ConversationDb::in_memory().unwrap();

        // Create a conversation
        let mut conv = Conversation::new("Test Chat", "alice");
        conv.add_participant(Participant::user("user:amy", "Amy"));
        conv.add_participant(Participant::model(
            "model:claude",
            "Claude",
            "anthropic",
            "claude-3-opus",
        ));
        conv.add_text_message("user:amy", "Hello!");

        let id = conv.id.clone();

        // Save
        db.save(&conv).unwrap();

        // Load
        let loaded = db.load(&id).unwrap().unwrap();
        assert_eq!(loaded.name, "Test Chat");
        assert_eq!(loaded.message_count(), 1);
        assert_eq!(loaded.participants.len(), 2);

        // Verify participant details
        let amy = loaded.get_participant("user:amy").unwrap();
        assert_eq!(amy.display_name, "Amy");
        assert!(amy.is_user());

        let claude = loaded.get_participant("model:claude").unwrap();
        assert_eq!(claude.display_name, "Claude");
        assert!(claude.is_model());
    }

    #[test]
    fn test_load_all() {
        let db = ConversationDb::in_memory().unwrap();

        // Create multiple conversations
        for i in 0..3 {
            let conv = Conversation::new(format!("Chat {}", i), "alice");
            db.save(&conv).unwrap();
        }

        let all = db.load_all().unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn test_delete() {
        let db = ConversationDb::in_memory().unwrap();

        let conv = Conversation::new("To Delete", "alice");
        let id = conv.id.clone();
        db.save(&conv).unwrap();

        assert!(db.exists(&id).unwrap());
        db.delete(&id).unwrap();
        assert!(!db.exists(&id).unwrap());
    }

    #[test]
    fn test_update() {
        let db = ConversationDb::in_memory().unwrap();

        let mut conv = Conversation::new("Original", "alice");
        let id = conv.id.clone();
        db.save(&conv).unwrap();

        // Update the conversation
        conv.name = "Updated".to_string();
        conv.add_text_message("alice", "New message");
        db.save(&conv).unwrap();

        let loaded = db.load(&id).unwrap().unwrap();
        assert_eq!(loaded.name, "Updated");
        assert_eq!(loaded.message_count(), 1);
    }

    #[test]
    fn test_mounts() {
        let db = ConversationDb::in_memory().unwrap();

        let mut conv = Conversation::new("With Mounts", "alice");
        conv.add_mount(Mount::read_only("kernel-1", "/project"));
        conv.add_mount(Mount::read_write("kernel-2", "/scratch"));

        let id = conv.id.clone();
        db.save(&conv).unwrap();

        let loaded = db.load(&id).unwrap().unwrap();
        assert_eq!(loaded.mounts.len(), 2);

        let project = loaded.get_mount("/project").unwrap();
        assert_eq!(project.kernel_id, "kernel-1");
        assert_eq!(project.access, AccessLevel::Read);

        let scratch = loaded.get_mount("/scratch").unwrap();
        assert_eq!(scratch.access, AccessLevel::ReadWrite);
    }
}
