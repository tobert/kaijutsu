//! SQLite persistence for conversations.
//!
//! Stores conversation metadata, participants, mounts, and normalized block data.
//! Uses relational tables instead of JSON blobs for schema evolution resilience.

use rusqlite::{params, Connection, Result as SqliteResult};
use std::path::Path;

use crate::conversation::{AccessLevel, Conversation, Mount, Participant, ParticipantKind};
use kaijutsu_crdt::{BlockDocument, BlockId, BlockKind, BlockSnapshot, DocumentSnapshot, Role, Status};
use kaijutsu_types::{ContextId, PrincipalId};

/// Database handle for conversation persistence.
pub struct ConversationDb {
    conn: Connection,
}

const SCHEMA: &str = r#"
-- Conversation metadata (normalized, no doc_encoded blob)
CREATE TABLE IF NOT EXISTS conversations (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL,
    cell_id TEXT NOT NULL UNIQUE,
    version INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_conversations_updated ON conversations(updated_at DESC);

-- Blocks (universal fields, composite PK)
CREATE TABLE IF NOT EXISTS blocks (
    cell_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    order_idx INTEGER NOT NULL,
    parent_cell_id TEXT,
    parent_agent_id TEXT,
    parent_seq INTEGER,
    role TEXT NOT NULL,
    status TEXT NOT NULL,
    kind TEXT NOT NULL,
    content TEXT NOT NULL,
    collapsed INTEGER NOT NULL DEFAULT 0,
    author TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (cell_id, agent_id, seq),
    FOREIGN KEY (cell_id) REFERENCES conversations(cell_id) ON DELETE CASCADE
);
CREATE INDEX IF NOT EXISTS idx_blocks_order ON blocks(cell_id, order_idx);

-- Tool calls (1:1 extension for kind='tool_call')
CREATE TABLE IF NOT EXISTS tool_calls (
    cell_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    tool_name TEXT NOT NULL,
    tool_input TEXT,
    PRIMARY KEY (cell_id, agent_id, seq),
    FOREIGN KEY (cell_id, agent_id, seq) REFERENCES blocks(cell_id, agent_id, seq) ON DELETE CASCADE
);

-- Tool results (1:1 extension for kind='tool_result')
CREATE TABLE IF NOT EXISTS tool_results (
    cell_id TEXT NOT NULL,
    agent_id TEXT NOT NULL,
    seq INTEGER NOT NULL,
    call_cell_id TEXT NOT NULL,
    call_agent_id TEXT NOT NULL,
    call_seq INTEGER NOT NULL,
    exit_code INTEGER,
    is_error INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (cell_id, agent_id, seq),
    FOREIGN KEY (cell_id, agent_id, seq) REFERENCES blocks(cell_id, agent_id, seq) ON DELETE CASCADE
);

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

// =============================================================================
// Row Structs (module-private helpers)
// =============================================================================

/// Maps a row from the blocks table.
#[derive(Debug)]
struct BlockRow {
    cell_id: String,
    agent_id: String,
    seq: u64,
    order_idx: i64,
    parent_cell_id: Option<String>,
    parent_agent_id: Option<String>,
    parent_seq: Option<u64>,
    role: String,
    status: String,
    kind: String,
    content: String,
    collapsed: bool,
    created_at: u64,
}

/// Maps a row from the tool_calls table.
#[derive(Debug)]
struct ToolCallRow {
    tool_name: String,
    tool_input: Option<String>,
}

/// Maps a row from the tool_results table.
#[derive(Debug)]
struct ToolResultRow {
    call_cell_id: String,
    call_agent_id: String,
    call_seq: u64,
    exit_code: Option<i32>,
    is_error: bool,
}

// =============================================================================
// Conversion Functions
// =============================================================================

/// Convert a BlockSnapshot to a BlockRow for insertion.
fn block_to_row(block: &BlockSnapshot, order_idx: usize) -> BlockRow {
    BlockRow {
        cell_id: block.id.context_id.to_hex(),
        agent_id: block.id.agent_id.to_hex(),
        seq: block.id.seq,
        order_idx: order_idx as i64,
        parent_cell_id: block.parent_id.as_ref().map(|p| p.context_id.to_hex()),
        parent_agent_id: block.parent_id.as_ref().map(|p| p.agent_id.to_hex()),
        parent_seq: block.parent_id.as_ref().map(|p| p.seq),
        role: block.role.as_str().to_string(),
        status: block.status.as_str().to_string(),
        kind: block.kind.as_str().to_string(),
        content: block.content.clone(),
        collapsed: block.collapsed,
        created_at: block.created_at,
    }
}

/// Convert a BlockRow (with optional extension data) back to a BlockSnapshot.
///
/// Returns `Err` if hex IDs in the row can't be parsed (corrupt DB data).
fn row_to_block(
    row: BlockRow,
    tool_call: Option<ToolCallRow>,
    tool_result: Option<ToolResultRow>,
) -> Result<BlockSnapshot, rusqlite::Error> {
    let context_id = ContextId::parse(&row.cell_id)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))?;
    let agent_id = PrincipalId::parse(&row.agent_id)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(1, rusqlite::types::Type::Text, Box::new(e)))?;
    let id = BlockId::new(context_id, agent_id, row.seq);

    let parent_id = match (&row.parent_cell_id, &row.parent_agent_id, row.parent_seq) {
        (Some(cell), Some(agent), Some(seq)) => {
            let p_ctx = ContextId::parse(cell)
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e)))?;
            let p_agent = PrincipalId::parse(agent)
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, Box::new(e)))?;
            Some(BlockId::new(p_ctx, p_agent, seq))
        }
        _ => None,
    };

    let role = Role::from_str(&row.role).unwrap_or_default();
    let status = Status::from_str(&row.status).unwrap_or_default();
    let kind = BlockKind::from_str(&row.kind).unwrap_or_default();

    // Extract tool-specific fields from extension rows
    let (tool_name, tool_input) = tool_call
        .map(|tc| {
            let input = tc.tool_input;
            (Some(tc.tool_name), input)
        })
        .unwrap_or((None, None));

    let (tool_call_id, exit_code, is_error) = match tool_result {
        Some(tr) => {
            let call_ctx = ContextId::parse(&tr.call_cell_id)
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, Box::new(e)))?;
            let call_agent = PrincipalId::parse(&tr.call_agent_id)
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, Box::new(e)))?;
            let call_id = BlockId::new(call_ctx, call_agent, tr.call_seq);
            (Some(call_id), tr.exit_code, tr.is_error)
        }
        None => (None, None, false),
    };

    Ok(BlockSnapshot {
        id,
        parent_id,
        role,
        status,
        kind,
        content: row.content,
        collapsed: row.collapsed,
        compacted: false,
        created_at: row.created_at,
        tool_kind: None,
        tool_name,
        tool_input,
        tool_call_id,
        exit_code,
        is_error,
        display_hint: None, // TODO: persist display hints to DB if needed
        source_context: None, // TODO: persist drift metadata to DB if needed
        source_model: None,
        drift_kind: None,
        file_path: None,
        order_key: None,
    })
}

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

    /// Save a conversation with its document (insert or replace).
    ///
    /// The document is optional - if None, only metadata is saved and existing
    /// blocks are preserved.
    pub fn save(&self, conv: &Conversation, doc: Option<&BlockDocument>) -> SqliteResult<()> {
        // Get cell_id and version from document, or use conversation id as fallback
        let (cell_id, version) = match doc {
            Some(d) => {
                let snapshot = d.snapshot();
                (snapshot.context_id.to_hex(), snapshot.version)
            }
            None => (conv.id.clone(), 0),
        };

        // Use a transaction for atomicity
        let tx = self.conn.unchecked_transaction()?;

        // Upsert conversation metadata
        tx.execute(
            "INSERT OR REPLACE INTO conversations (id, name, cell_id, version, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                conv.id,
                conv.name,
                cell_id,
                version as i64,
                conv.created_at as i64,
                conv.updated_at as i64,
            ],
        )?;

        // Save blocks only if document is provided
        if let Some(d) = doc {
            // Delete existing blocks for this cell_id (cascade handles extension tables)
            tx.execute("DELETE FROM blocks WHERE cell_id = ?1", params![cell_id])?;

            // Insert each block with order index
            let snapshot = d.snapshot();
            for (order_idx, block) in snapshot.blocks.iter().enumerate() {
                self.save_block(&tx, block, order_idx)?;
            }
        }

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

    /// Save a single block with its extension data.
    fn save_block(
        &self,
        tx: &rusqlite::Transaction<'_>,
        block: &BlockSnapshot,
        order_idx: usize,
    ) -> SqliteResult<()> {
        let row = block_to_row(block, order_idx);

        // Insert base block row (author column = agent_id, derived from block.id.agent_id)
        tx.execute(
            "INSERT INTO blocks (
                cell_id, agent_id, seq, order_idx,
                parent_cell_id, parent_agent_id, parent_seq,
                role, status, kind, content, collapsed, author, created_at
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                row.cell_id,
                row.agent_id,
                row.seq as i64,
                row.order_idx,
                row.parent_cell_id,
                row.parent_agent_id,
                row.parent_seq.map(|s| s as i64),
                row.role,
                row.status,
                row.kind,
                row.content,
                row.collapsed as i32,
                row.agent_id, // author = agent_id (no separate author field in BlockSnapshot)
                row.created_at as i64,
            ],
        )?;

        // Insert extension row if applicable
        match block.kind {
            BlockKind::ToolCall => {
                if let Some(ref tool_name) = block.tool_name {
                    let tool_input = block.tool_input.as_deref();
                    tx.execute(
                        "INSERT INTO tool_calls (cell_id, agent_id, seq, tool_name, tool_input)
                         VALUES (?1, ?2, ?3, ?4, ?5)",
                        params![
                            row.cell_id,
                            row.agent_id,
                            row.seq as i64,
                            tool_name,
                            tool_input,
                        ],
                    )?;
                }
            }
            BlockKind::ToolResult => {
                if let Some(ref call_id) = block.tool_call_id {
                    tx.execute(
                        "INSERT INTO tool_results (
                            cell_id, agent_id, seq,
                            call_cell_id, call_agent_id, call_seq,
                            exit_code, is_error
                        ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                        params![
                            row.cell_id,
                            row.agent_id,
                            row.seq as i64,
                            call_id.context_id.to_hex(),
                            call_id.agent_id.to_hex(),
                            call_id.seq as i64,
                            block.exit_code,
                            block.is_error as i32,
                        ],
                    )?;
                }
            }
            _ => {}
        }

        Ok(())
    }

    /// Load a conversation and its document by ID.
    ///
    /// Returns the conversation metadata and optionally the document (if blocks exist).
    pub fn load(&self, id: &str) -> SqliteResult<Option<(Conversation, Option<BlockDocument>)>> {
        // Load conversation metadata
        let mut stmt = self.conn.prepare(
            "SELECT id, name, cell_id, version, created_at, updated_at
             FROM conversations WHERE id = ?1",
        )?;

        let mut rows = stmt.query(params![id])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };

        let conv_id: String = row.get(0)?;
        let name: String = row.get(1)?;
        let cell_id: String = row.get(2)?;
        let version: i64 = row.get(3)?;
        let created_at: i64 = row.get(4)?;
        let updated_at: i64 = row.get(5)?;

        // Load blocks and reconstruct document if any exist
        let blocks = self.load_blocks(&cell_id)?;
        let doc = if blocks.is_empty() {
            None
        } else {
            let context_id = ContextId::parse(&cell_id)
                .map_err(|e| rusqlite::Error::FromSqlConversionFailure(2, rusqlite::types::Type::Text, Box::new(e)))?;
            let snapshot = DocumentSnapshot {
                context_id,
                blocks,
                version: version as u64,
            };
            Some(BlockDocument::from_snapshot(snapshot, PrincipalId::system()))
        };

        // Load participants
        let participants = self.load_participants(&conv_id)?;

        // Load mounts
        let mounts = self.load_mounts(&conv_id)?;

        let conv = Conversation {
            id: conv_id,
            name,
            participants,
            mounts,
            created_at: created_at as u64,
            updated_at: updated_at as u64,
        };

        Ok(Some((conv, doc)))
    }

    /// Load all blocks for a cell_id, ordered by order_idx.
    fn load_blocks(&self, cell_id: &str) -> SqliteResult<Vec<BlockSnapshot>> {
        let mut stmt = self.conn.prepare(
            "SELECT cell_id, agent_id, seq, order_idx,
                    parent_cell_id, parent_agent_id, parent_seq,
                    role, status, kind, content, collapsed, author, created_at
             FROM blocks WHERE cell_id = ?1 ORDER BY order_idx",
        )?;

        let rows = stmt.query_map(params![cell_id], |row| {
            Ok(BlockRow {
                cell_id: row.get(0)?,
                agent_id: row.get(1)?,
                seq: row.get::<_, i64>(2)? as u64,
                order_idx: row.get(3)?,
                parent_cell_id: row.get(4)?,
                parent_agent_id: row.get(5)?,
                parent_seq: row.get::<_, Option<i64>>(6)?.map(|s| s as u64),
                role: row.get(7)?,
                status: row.get(8)?,
                kind: row.get(9)?,
                content: row.get(10)?,
                collapsed: row.get::<_, i32>(11)? != 0,
                // column 12 is `author` — skipped, derived from agent_id
                created_at: row.get::<_, i64>(13)? as u64,
            })
        })?;

        let mut blocks = Vec::new();
        for row_result in rows {
            let row = row_result?;
            let kind = BlockKind::from_str(&row.kind).unwrap_or_default();

            // Load extension data based on kind
            let tool_call = if kind == BlockKind::ToolCall {
                self.load_tool_call(&row.cell_id, &row.agent_id, row.seq)?
            } else {
                None
            };

            let tool_result = if kind == BlockKind::ToolResult {
                self.load_tool_result(&row.cell_id, &row.agent_id, row.seq)?
            } else {
                None
            };

            blocks.push(row_to_block(row, tool_call, tool_result)?);
        }

        Ok(blocks)
    }

    /// Load tool_call extension data by PK.
    fn load_tool_call(
        &self,
        cell_id: &str,
        agent_id: &str,
        seq: u64,
    ) -> SqliteResult<Option<ToolCallRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT tool_name, tool_input FROM tool_calls
             WHERE cell_id = ?1 AND agent_id = ?2 AND seq = ?3",
        )?;

        let mut rows = stmt.query(params![cell_id, agent_id, seq as i64])?;
        match rows.next()? {
            Some(row) => Ok(Some(ToolCallRow {
                tool_name: row.get(0)?,
                tool_input: row.get(1)?,
            })),
            None => Ok(None),
        }
    }

    /// Load tool_result extension data by PK.
    fn load_tool_result(
        &self,
        cell_id: &str,
        agent_id: &str,
        seq: u64,
    ) -> SqliteResult<Option<ToolResultRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT call_cell_id, call_agent_id, call_seq, exit_code, is_error
             FROM tool_results
             WHERE cell_id = ?1 AND agent_id = ?2 AND seq = ?3",
        )?;

        let mut rows = stmt.query(params![cell_id, agent_id, seq as i64])?;
        match rows.next()? {
            Some(row) => Ok(Some(ToolResultRow {
                call_cell_id: row.get(0)?,
                call_agent_id: row.get(1)?,
                call_seq: row.get::<_, i64>(2)? as u64,
                exit_code: row.get(3)?,
                is_error: row.get::<_, i32>(4)? != 0,
            })),
            None => Ok(None),
        }
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
    pub fn load_all(&self) -> SqliteResult<Vec<(Conversation, Option<BlockDocument>)>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM conversations ORDER BY updated_at DESC")?;

        let ids: Vec<String> = stmt
            .query_map([], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();

        let mut conversations = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(result) = self.load(&id)? {
                conversations.push(result);
            }
        }

        Ok(conversations)
    }

    /// Delete a conversation.
    pub fn delete(&self, id: &str) -> SqliteResult<()> {
        // Foreign key cascade handles participants, mounts, and blocks
        self.conn
            .execute("DELETE FROM conversations WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// List conversation IDs (without loading full content).
    pub fn list_ids(&self) -> SqliteResult<Vec<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM conversations ORDER BY updated_at DESC")?;

        let rows = stmt.query_map([], |row| row.get(0))?;
        rows.collect()
    }

    /// Get conversation count.
    pub fn count(&self) -> SqliteResult<usize> {
        self.conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |row| {
                row.get::<_, i64>(0)
            })
            .map(|c| c as usize)
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

    /// Helper: create a document with some messages.
    fn create_test_doc() -> BlockDocument {
        let mut doc = BlockDocument::new(ContextId::new(), PrincipalId::new());
        doc.insert_block(None, None, Role::User, BlockKind::Text, "Hello!")
            .unwrap();
        doc
    }

    #[test]
    fn test_save_and_load() {
        let db = ConversationDb::in_memory().unwrap();

        // Create a conversation with participants
        let mut conv = Conversation::new("Test Chat");
        conv.add_participant(Participant::user("user:amy", "Amy"));
        conv.add_participant(Participant::model(
            "model:claude",
            "Claude",
            "anthropic",
            "claude-3-opus",
        ));

        let id = conv.id.clone();

        // Create a document with a message
        let doc = create_test_doc();

        // Save
        db.save(&conv, Some(&doc)).unwrap();

        // Load
        let (loaded_conv, loaded_doc) = db.load(&id).unwrap().unwrap();
        assert_eq!(loaded_conv.name, "Test Chat");
        assert_eq!(loaded_conv.participants.len(), 2);

        // Verify document was loaded
        let loaded_doc = loaded_doc.expect("document should exist");
        assert_eq!(loaded_doc.block_count(), 1);

        // Verify participant details
        let amy = loaded_conv.get_participant("user:amy").unwrap();
        assert_eq!(amy.display_name, "Amy");
        assert!(amy.is_user());

        let claude = loaded_conv.get_participant("model:claude").unwrap();
        assert_eq!(claude.display_name, "Claude");
        assert!(claude.is_model());
    }

    #[test]
    fn test_load_all() {
        let db = ConversationDb::in_memory().unwrap();

        // Create multiple conversations (metadata only)
        for i in 0..3 {
            let conv = Conversation::new(format!("Chat {}", i));
            db.save(&conv, None).unwrap();
        }

        let all = db.load_all().unwrap();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn test_delete() {
        let db = ConversationDb::in_memory().unwrap();

        let conv = Conversation::new("To Delete");
        let id = conv.id.clone();
        db.save(&conv, None).unwrap();

        assert!(db.exists(&id).unwrap());
        db.delete(&id).unwrap();
        assert!(!db.exists(&id).unwrap());
    }

    #[test]
    fn test_update() {
        let db = ConversationDb::in_memory().unwrap();

        let mut conv = Conversation::new("Original");
        let id = conv.id.clone();
        db.save(&conv, None).unwrap();

        // Update the conversation name
        conv.name = "Updated".to_string();

        // Create a document with a message
        let doc = create_test_doc();
        db.save(&conv, Some(&doc)).unwrap();

        let (loaded_conv, loaded_doc) = db.load(&id).unwrap().unwrap();
        assert_eq!(loaded_conv.name, "Updated");
        assert_eq!(loaded_doc.unwrap().block_count(), 1);
    }

    #[test]
    fn test_mounts() {
        let db = ConversationDb::in_memory().unwrap();

        let mut conv = Conversation::new("With Mounts");
        conv.add_mount(Mount::read_only("kernel-1", "/project"));
        conv.add_mount(Mount::read_write("kernel-2", "/scratch"));

        let id = conv.id.clone();
        db.save(&conv, None).unwrap();

        let (loaded_conv, _) = db.load(&id).unwrap().unwrap();
        assert_eq!(loaded_conv.mounts.len(), 2);

        let project = loaded_conv.get_mount("/project").unwrap();
        assert_eq!(project.kernel_id, "kernel-1");
        assert_eq!(project.access, AccessLevel::Read);

        let scratch = loaded_conv.get_mount("/scratch").unwrap();
        assert_eq!(scratch.access, AccessLevel::ReadWrite);
    }

    #[test]
    fn test_tool_call_persistence() {
        let db = ConversationDb::in_memory().unwrap();

        let conv = Conversation::new("Tool Test");
        let id = conv.id.clone();

        // Create document with a tool call
        let mut doc = BlockDocument::new(ContextId::new(), PrincipalId::new());
        let tool_input = serde_json::json!({"path": "/etc/hosts", "recursive": true});
        doc.insert_tool_call(None, None, "read_file", tool_input.clone())
            .unwrap();

        db.save(&conv, Some(&doc)).unwrap();

        // Load and verify
        let (_, loaded_doc) = db.load(&id).unwrap().unwrap();
        let loaded_doc = loaded_doc.expect("document should exist");
        let blocks = loaded_doc.blocks_ordered();
        assert_eq!(blocks.len(), 1);

        let block = &blocks[0];
        assert_eq!(block.kind, BlockKind::ToolCall);
        assert_eq!(block.tool_name, Some("read_file".to_string()));
        assert_eq!(block.tool_input, Some(serde_json::to_string_pretty(&tool_input).unwrap()));
    }

    #[test]
    fn test_tool_result_persistence() {
        let db = ConversationDb::in_memory().unwrap();

        let conv = Conversation::new("Tool Result Test");
        let id = conv.id.clone();

        // Create document with tool call and result
        let mut doc = BlockDocument::new(ContextId::new(), PrincipalId::new());
        let tool_input = serde_json::json!({"command": "ls -la"});
        let call_id = doc
            .insert_tool_call(None, None, "bash", tool_input)
            .unwrap();
        doc.insert_tool_result_block(
            &call_id,
            None,
            "total 4\ndrwxr-xr-x 2 user user 4096 Jan 1 00:00 .",
            false,
            Some(0),
        )
        .unwrap();

        db.save(&conv, Some(&doc)).unwrap();

        // Load and verify
        let (_, loaded_doc) = db.load(&id).unwrap().unwrap();
        let loaded_doc = loaded_doc.expect("document should exist");
        let blocks = loaded_doc.blocks_ordered();
        assert_eq!(blocks.len(), 2);

        let result = &blocks[1];
        assert_eq!(result.kind, BlockKind::ToolResult);
        assert_eq!(result.tool_call_id, Some(call_id));
        assert_eq!(result.exit_code, Some(0));
        assert!(!result.is_error);
    }

    #[test]
    fn test_tool_result_with_error() {
        let db = ConversationDb::in_memory().unwrap();

        let conv = Conversation::new("Error Test");
        let id = conv.id.clone();

        // Create document with tool call and error result
        let mut doc = BlockDocument::new(ContextId::new(), PrincipalId::new());
        let tool_input = serde_json::json!({"command": "cat /nonexistent"});
        let call_id = doc
            .insert_tool_call(None, None, "bash", tool_input)
            .unwrap();
        doc.insert_tool_result_block(
            &call_id,
            None,
            "cat: /nonexistent: No such file or directory",
            true,
            Some(1),
        )
        .unwrap();

        db.save(&conv, Some(&doc)).unwrap();

        // Load and verify
        let (_, loaded_doc) = db.load(&id).unwrap().unwrap();
        let loaded_doc = loaded_doc.expect("document should exist");
        let blocks = loaded_doc.blocks_ordered();

        let result = &blocks[1];
        assert_eq!(result.kind, BlockKind::ToolResult);
        assert!(result.is_error);
        assert_eq!(result.exit_code, Some(1));
    }

    #[test]
    fn test_multiple_blocks_ordering() {
        let db = ConversationDb::in_memory().unwrap();

        let conv = Conversation::new("Ordering Test");
        let id = conv.id.clone();

        // Create document with multiple blocks
        let mut doc = BlockDocument::new(ContextId::new(), PrincipalId::new());
        doc.insert_block(None, None, Role::User, BlockKind::Text, "First")
            .unwrap();
        let last = doc.blocks_ordered().last().map(|b| b.id);
        doc.insert_block(None, last.as_ref(), Role::Model, BlockKind::Text, "Second")
            .unwrap();
        let last = doc.blocks_ordered().last().map(|b| b.id);
        doc.insert_block(None, last.as_ref(), Role::User, BlockKind::Text, "Third")
            .unwrap();

        db.save(&conv, Some(&doc)).unwrap();

        // Load and verify order
        let (_, loaded_doc) = db.load(&id).unwrap().unwrap();
        let loaded_doc = loaded_doc.expect("document should exist");
        let blocks = loaded_doc.blocks_ordered();
        assert_eq!(blocks.len(), 3);
        assert_eq!(blocks[0].content, "First");
        assert_eq!(blocks[1].content, "Second");
        assert_eq!(blocks[2].content, "Third");
    }
}
