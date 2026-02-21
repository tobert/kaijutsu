//! Per-block DTE content with metadata.
//!
//! Each block owns its own diamond-types-extended `Document` for content,
//! plus a `BlockHeader` for metadata (kind, role, status, parent_id, etc.).
//! This replaces the shared-Document model where all blocks lived as paths
//! in a single DTE Document.

use diamond_types_extended::{AgentId, Document, Frontier, SerializedOpsOwned, Uuid};

use crate::{BlockHeader, BlockId, BlockSnapshot, PrincipalId, Status};

/// Base-62 charset for fractional indexing (0-9, A-Z, a-z).
/// Lexicographically ordered: '0' < '9' < 'A' < 'Z' < 'a' < 'z'.
pub const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Get the index of a character in the BASE62 charset.
fn base62_index(c: u8) -> usize {
    BASE62.iter().position(|&b| b == c).unwrap_or(0)
}

/// Compute a lexicographic midpoint between two base-62 strings.
///
/// Empty string `""` sorts before everything. Both `a` and `b` must satisfy `a < b`
/// lexicographically. The result is guaranteed to satisfy `a < result < b`.
pub(crate) fn order_midpoint(a: &str, b: &str) -> String {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let max_len = a_bytes.len().max(b_bytes.len());

    let mut result = Vec::new();

    for i in 0..=max_len {
        let a_val = if i < a_bytes.len() { base62_index(a_bytes[i]) } else { 0 };
        let b_val = if i < b_bytes.len() { base62_index(b_bytes[i]) } else { 62 };

        if a_val + 1 < b_val {
            let mid = (a_val + b_val) / 2;
            result.push(BASE62[mid]);
            return String::from_utf8(result).unwrap_or_else(|_| "V".to_string());
        } else if a_val == b_val {
            result.push(BASE62[a_val]);
        } else {
            result.push(BASE62[a_val]);
            let a_next = if i + 1 < a_bytes.len() { base62_index(a_bytes[i + 1]) } else { 0 };
            let mid = (a_next + 62) / 2;
            result.push(BASE62[mid]);
            return String::from_utf8(result).unwrap_or_else(|_| "V".to_string());
        }
    }

    result.push(BASE62[31]); // 'V'
    String::from_utf8(result).unwrap_or_else(|_| "V".to_string())
}

/// A single block's content and metadata.
///
/// The DTE Document holds the block's text content (character-level CRDT).
/// The BlockHeader holds identity, kind, role, status, etc. (plain data).
/// The order_key determines sibling ordering (fractional index).
pub struct BlockContent {
    /// Block identity + metadata.
    header: BlockHeader,

    /// This block's own DTE Document instance — just for content.
    /// Text blocks: DTE Text CRDT (character-level editing).
    /// ToolCall blocks: DTE Text for tool_input (streamable JSON).
    /// File blocks: DTE Text (full file content).
    doc: Document,

    /// DTE agent ID for this replica.
    agent: AgentId,

    /// Fractional index for sibling ordering (base-62 lexicographic).
    /// Calculated via order_midpoint() on insertion.
    order_key: String,

    /// Non-Copy snapshot fields that don't belong on BlockHeader.
    /// These are write-once metadata set at creation time.
    tool_name: Option<String>,
    tool_input: Option<String>,
    tool_call_id: Option<BlockId>,
    display_hint: Option<String>,
    source_context: Option<crate::ContextId>,
    source_model: Option<String>,
    drift_kind: Option<crate::DriftKind>,
    file_path: Option<String>,

    /// Whether this block is collapsed (only meaningful for Thinking blocks).
    collapsed: bool,

    /// Whether this block has been deleted (tombstone).
    deleted: bool,
}

impl BlockContent {
    /// Create a new block with empty content.
    pub fn new(header: BlockHeader, agent_id: PrincipalId, order_key: String) -> Self {
        let mut doc = Document::new();
        let dte_uuid = Uuid::from_bytes(*agent_id.as_bytes());
        let agent = doc.create_agent(dte_uuid);

        // Create the text CRDT for content
        doc.transact(agent, |tx| {
            tx.root().create_text("content");
        });

        Self {
            header,
            doc,
            agent,
            order_key,
            tool_name: None,
            tool_input: None,
            tool_call_id: None,
            display_hint: None,
            source_context: None,
            source_model: None,
            drift_kind: None,
            file_path: None,
            collapsed: false,
            deleted: false,
        }
    }

    /// Create a new block with initial text content.
    pub fn with_content(
        header: BlockHeader,
        content: &str,
        agent_id: PrincipalId,
        order_key: String,
    ) -> Self {
        let mut block = Self::new(header, agent_id, order_key);
        if !content.is_empty() {
            block.doc.transact(block.agent, |tx| {
                if let Some(mut text) = tx.get_text_mut(&["content"]) {
                    text.insert(0, content);
                }
            });
        }
        block
    }

    /// Create from a full BlockSnapshot (for restoring from persistence).
    ///
    /// If the snapshot carries an `order_key`, it is used; otherwise falls back
    /// to `fallback_order_key`.
    pub fn from_snapshot(snap: &BlockSnapshot, agent_id: PrincipalId, fallback_order_key: String) -> Self {
        let header = BlockHeader::from_snapshot(snap);
        let order_key = snap.order_key.clone().unwrap_or(fallback_order_key);
        let mut block = Self::with_content(header, &snap.content, agent_id, order_key);
        block.tool_name = snap.tool_name.clone();
        block.tool_input = snap.tool_input.clone();
        block.tool_call_id = snap.tool_call_id;
        block.display_hint = snap.display_hint.clone();
        block.source_context = snap.source_context;
        block.source_model = snap.source_model.clone();
        block.drift_kind = snap.drift_kind;
        block.file_path = snap.file_path.clone();
        block.collapsed = snap.collapsed;
        block
    }

    // ── Content access ──────────────────────────────────────────────────

    /// Get the current text content.
    pub fn text(&self) -> String {
        self.doc
            .get_text(&["content"])
            .map(|t| t.content())
            .unwrap_or_default()
    }

    /// Edit text at a position (insert and/or delete).
    pub fn edit_text(&mut self, pos: usize, insert: &str, delete: usize) {
        self.doc.transact(self.agent, |tx| {
            if let Some(mut text) = tx.get_text_mut(&["content"]) {
                if delete > 0 {
                    text.delete(pos..pos + delete);
                }
                if !insert.is_empty() {
                    text.insert(pos, insert);
                }
            }
        });
    }

    /// Append text to the end.
    pub fn append_text(&mut self, text: &str) {
        let len = self.text().chars().count();
        self.edit_text(len, text, 0);
    }

    /// Get the character count of the content.
    pub fn content_len(&self) -> usize {
        self.text().chars().count()
    }

    // ── Metadata access ─────────────────────────────────────────────────

    /// Get the block ID.
    pub fn id(&self) -> BlockId {
        self.header.id
    }

    /// Get an immutable reference to the header.
    pub fn header(&self) -> &BlockHeader {
        &self.header
    }

    /// Get the ordering key.
    pub fn order_key(&self) -> &str {
        &self.order_key
    }

    /// Set the ordering key (for move operations).
    pub fn set_order_key(&mut self, key: String) {
        self.order_key = key;
    }

    /// Set the status, bumping updated_at with Lamport timestamp.
    pub fn set_status(&mut self, status: Status, lamport_ts: u64) {
        self.header.status = status;
        self.header.updated_at = lamport_ts;
    }

    /// Set collapsed state, bumping updated_at with Lamport timestamp.
    pub fn set_collapsed(&mut self, collapsed: bool, lamport_ts: u64) {
        self.collapsed = collapsed;
        self.header.collapsed = collapsed;
        self.header.updated_at = lamport_ts;
    }

    /// Whether this block is deleted (tombstone).
    pub fn is_deleted(&self) -> bool {
        self.deleted
    }

    /// Mark as deleted (tombstone).
    pub fn mark_deleted(&mut self, lamport_ts: u64) {
        self.deleted = true;
        self.header.updated_at = lamport_ts;
    }

    // ── Snapshot fields ─────────────────────────────────────────────────

    pub fn tool_name(&self) -> Option<&str> {
        self.tool_name.as_deref()
    }

    pub fn set_tool_name(&mut self, name: Option<String>) {
        self.tool_name = name;
    }

    pub fn tool_input(&self) -> Option<&str> {
        self.tool_input.as_deref()
    }

    pub fn set_tool_input(&mut self, input: Option<String>) {
        self.tool_input = input;
    }

    pub fn tool_call_id(&self) -> Option<BlockId> {
        self.tool_call_id
    }

    pub fn set_tool_call_id(&mut self, id: Option<BlockId>) {
        self.tool_call_id = id;
    }

    pub fn display_hint(&self) -> Option<&str> {
        self.display_hint.as_deref()
    }

    pub fn set_display_hint(&mut self, hint: Option<String>) {
        self.display_hint = hint;
    }

    pub fn source_context(&self) -> Option<crate::ContextId> {
        self.source_context
    }

    pub fn source_model(&self) -> Option<&str> {
        self.source_model.as_deref()
    }

    pub fn drift_kind(&self) -> Option<crate::DriftKind> {
        self.drift_kind
    }

    pub fn file_path(&self) -> Option<&str> {
        self.file_path.as_deref()
    }

    pub fn set_file_path(&mut self, path: Option<String>) {
        self.file_path = path;
    }

    // ── Sync ────────────────────────────────────────────────────────────

    /// Get operations since a frontier (per-block sync).
    pub fn ops_since(&self, frontier: &Frontier) -> SerializedOpsOwned {
        self.doc.ops_since_owned(frontier)
    }

    /// Merge remote operations into this block's content.
    pub fn merge_ops(&mut self, ops: SerializedOpsOwned) -> crate::Result<()> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.doc.merge_ops(ops)
        }));
        match result {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(crate::CrdtError::Internal(format!("block merge error: {:?}", e))),
            Err(_) => Err(crate::CrdtError::Internal(
                "block CRDT merge panicked".into(),
            )),
        }
    }

    /// Get the current frontier (per-block version).
    pub fn frontier(&self) -> Frontier {
        self.doc.version().clone()
    }

    // ── Snapshot ─────────────────────────────────────────────────────────

    /// Freeze this block into a BlockSnapshot.
    pub fn snapshot(&self) -> BlockSnapshot {
        BlockSnapshot {
            id: self.header.id,
            parent_id: self.header.parent_id,
            role: self.header.role,
            status: self.header.status,
            kind: self.header.kind,
            content: self.text(),
            collapsed: self.collapsed,
            compacted: self.header.compacted,
            created_at: self.header.created_at,
            tool_kind: self.header.tool_kind,
            tool_name: self.tool_name.clone(),
            tool_input: self.tool_input.clone(),
            tool_call_id: self.tool_call_id,
            exit_code: self.header.exit_code,
            is_error: self.header.is_error,
            display_hint: self.display_hint.clone(),
            source_context: self.source_context,
            source_model: self.source_model.clone(),
            drift_kind: self.drift_kind,
            file_path: self.file_path.clone(),
            order_key: Some(self.order_key.clone()),
        }
    }

    /// Merge a remote header (LWW by updated_at, agent_id tiebreak).
    pub fn merge_header(&mut self, remote: &BlockHeader) {
        if remote.updated_at > self.header.updated_at
            || (remote.updated_at == self.header.updated_at
                && remote.id.agent_id > self.header.id.agent_id)
        {
            self.header.status = remote.status;
            self.header.compacted = remote.compacted;
            self.header.collapsed = remote.collapsed;
            self.collapsed = remote.collapsed;
            self.header.updated_at = remote.updated_at;
            self.header.tool_kind = remote.tool_kind;
            self.header.exit_code = remote.exit_code;
            self.header.is_error = remote.is_error;
        }
    }
}
