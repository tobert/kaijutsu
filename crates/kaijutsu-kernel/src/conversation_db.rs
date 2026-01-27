//! SQLite persistence for conversations.
//!
//! Stores conversation metadata, participants, mounts, and normalized block data.
//! Uses relational tables instead of JSON blobs for schema evolution resilience.

use rusqlite::{params, Connection, Result as SqliteResult};
use std::path::Path;

use crate::conversation::{AccessLevel, Conversation, Mount, Participant, ParticipantKind};
use kaijutsu_crdt::{BlockDocument, BlockId, BlockKind, BlockSnapshot, DocumentSnapshot, Role, Status};

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
    author: String,
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
        cell_id: block.id.document_id.clone(),
        agent_id: block.id.agent_id.clone(),
        seq: block.id.seq,
        order_idx: order_idx as i64,
        parent_cell_id: block.parent_id.as_ref().map(|p| p.document_id.clone()),
        parent_agent_id: block.parent_id.as_ref().map(|p| p.agent_id.clone()),
        parent_seq: block.parent_id.as_ref().map(|p| p.seq),
        role: block.role.as_str().to_string(),
        status: block.status.as_str().to_string(),
        kind: block.kind.as_str().to_string(),
        content: block.content.clone(),
        collapsed: block.collapsed,
        author: block.author.clone(),
        created_at: block.created_at,
    }
}

/// Convert a BlockRow (with optional extension data) back to a BlockSnapshot.
fn row_to_block(
    row: BlockRow,
    tool_call: Option<ToolCallRow>,
    tool_result: Option<ToolResultRow>,
) -> BlockSnapshot {
    let id = BlockId::new(&row.cell_id, &row.agent_id, row.seq);

    let parent_id = match (&row.parent_cell_id, &row.parent_agent_id, row.parent_seq) {
        (Some(cell), Some(agent), Some(seq)) => Some(BlockId::new(cell, agent, seq)),
        _ => None,
    };

    let role = Role::from_str(&row.role).unwrap_or_default();
    let status = Status::from_str(&row.status).unwrap_or_default();
    let kind = BlockKind::from_str(&row.kind).unwrap_or_default();

    // Extract tool-specific fields from extension rows
    let (tool_name, tool_input) = tool_call
        .map(|tc| {
            let input = tc.tool_input.and_then(|s| serde_json::from_str(&s).ok());
            (Some(tc.tool_name), input)
        })
        .unwrap_or((None, None));

    let (tool_call_id, exit_code, is_error) = tool_result
        .map(|tr| {
            let call_id = BlockId::new(&tr.call_cell_id, &tr.call_agent_id, tr.call_seq);
            (Some(call_id), tr.exit_code, tr.is_error)
        })
        .unwrap_or((None, None, false));

    BlockSnapshot {
        id,
        parent_id,
        role,
        status,
        kind,
        content: row.content,
        collapsed: row.collapsed,
        author: row.author,
        created_at: row.created_at,
        tool_name,
        tool_input,
        tool_call_id,
        exit_code,
        is_error,
        display_hint: None, // TODO: persist display hints to DB if needed
    }
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

    /// Save a conversation (insert or replace).
    pub fn save(&self, conv: &Conversation) -> SqliteResult<()> {
        let snapshot = conv.doc.snapshot();
        let cell_id = snapshot.document_id.clone();
        let version = snapshot.version;

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

        // Delete existing blocks for this cell_id (cascade handles extension tables)
        tx.execute("DELETE FROM blocks WHERE cell_id = ?1", params![cell_id])?;

        // Insert each block with order index
        let blocks = snapshot.blocks;
        for (order_idx, block) in blocks.iter().enumerate() {
            self.save_block(&tx, block, order_idx)?;
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

        // Insert base block row
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
                row.author,
                row.created_at as i64,
            ],
        )?;

        // Insert extension row if applicable
        match block.kind {
            BlockKind::ToolCall => {
                if let Some(ref tool_name) = block.tool_name {
                    let tool_input = block
                        .tool_input
                        .as_ref()
                        .map(|v| serde_json::to_string(v).unwrap_or_default());
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
                            call_id.document_id,
                            call_id.agent_id,
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

    /// Load a conversation by ID.
    pub fn load(&self, id: &str) -> SqliteResult<Option<Conversation>> {
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

        // Load blocks and reconstruct document
        let blocks = self.load_blocks(&cell_id)?;
        let snapshot = DocumentSnapshot {
            document_id: cell_id,
            blocks,
            version: version as u64,
        };
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
                author: row.get(12)?,
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

            blocks.push(row_to_block(row, tool_call, tool_result));
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
    pub fn load_all(&self) -> SqliteResult<Vec<Conversation>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id FROM conversations ORDER BY updated_at DESC")?;

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

    #[test]
    fn test_tool_call_persistence() {
        let db = ConversationDb::in_memory().unwrap();

        let mut conv = Conversation::new("Tool Test", "alice");
        conv.add_participant(Participant::model(
            "model:claude",
            "Claude",
            "anthropic",
            "claude-3-opus",
        ));

        // Add a tool call
        let tool_input = serde_json::json!({"path": "/etc/hosts", "recursive": true});
        let call_id = conv.add_tool_call("model:claude", "read_file", tool_input.clone());
        assert!(call_id.is_some());

        let id = conv.id.clone();
        db.save(&conv).unwrap();

        // Load and verify
        let loaded = db.load(&id).unwrap().unwrap();
        let messages = loaded.messages();
        assert_eq!(messages.len(), 1);

        let block = &messages[0];
        assert_eq!(block.kind, BlockKind::ToolCall);
        assert_eq!(block.tool_name, Some("read_file".to_string()));
        assert_eq!(block.tool_input, Some(tool_input));
    }

    #[test]
    fn test_tool_result_persistence() {
        let db = ConversationDb::in_memory().unwrap();

        let mut conv = Conversation::new("Tool Result Test", "alice");
        conv.add_participant(Participant::model(
            "model:claude",
            "Claude",
            "anthropic",
            "claude-3-opus",
        ));

        // Add a tool call and result
        let tool_input = serde_json::json!({"command": "ls -la"});
        let call_id = conv
            .add_tool_call("model:claude", "bash", tool_input)
            .unwrap();
        let result_id = conv.add_tool_result(
            &call_id,
            "total 4\ndrwxr-xr-x 2 user user 4096 Jan 1 00:00 .",
            false,
            Some(0),
            "system",
        );
        assert!(result_id.is_some());

        let id = conv.id.clone();
        db.save(&conv).unwrap();

        // Load and verify
        let loaded = db.load(&id).unwrap().unwrap();
        let messages = loaded.messages();
        assert_eq!(messages.len(), 2);

        let result = &messages[1];
        assert_eq!(result.kind, BlockKind::ToolResult);
        assert_eq!(result.tool_call_id, Some(call_id));
        assert_eq!(result.exit_code, Some(0));
        assert!(!result.is_error);
    }

    #[test]
    fn test_tool_result_with_error() {
        let db = ConversationDb::in_memory().unwrap();

        let mut conv = Conversation::new("Error Test", "alice");
        conv.add_participant(Participant::model(
            "model:claude",
            "Claude",
            "anthropic",
            "claude-3-opus",
        ));

        // Add a tool call and error result
        let tool_input = serde_json::json!({"command": "cat /nonexistent"});
        let call_id = conv
            .add_tool_call("model:claude", "bash", tool_input)
            .unwrap();
        conv.add_tool_result(
            &call_id,
            "cat: /nonexistent: No such file or directory",
            true,
            Some(1),
            "system",
        );

        let id = conv.id.clone();
        db.save(&conv).unwrap();

        // Load and verify
        let loaded = db.load(&id).unwrap().unwrap();
        let messages = loaded.messages();

        let result = &messages[1];
        assert_eq!(result.kind, BlockKind::ToolResult);
        assert!(result.is_error);
        assert_eq!(result.exit_code, Some(1));
    }

    #[test]
    fn test_multiple_blocks_ordering() {
        let db = ConversationDb::in_memory().unwrap();

        let mut conv = Conversation::new("Ordering Test", "alice");
        conv.add_text_message("user:amy", "First");
        conv.add_text_message("model:claude", "Second");
        conv.add_text_message("user:amy", "Third");

        let id = conv.id.clone();
        db.save(&conv).unwrap();

        // Load and verify order
        let loaded = db.load(&id).unwrap().unwrap();
        let messages = loaded.messages();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0].content, "First");
        assert_eq!(messages[1].content, "Second");
        assert_eq!(messages[2].content, "Third");
    }
}
