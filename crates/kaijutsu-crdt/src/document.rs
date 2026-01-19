//! Block document with unified CRDT using diamond-types.
//!
//! Uses the diamond-types fork (github.com/tobert/diamond-types, branch feat/maps-and-uuids)
//! which provides unified OpLog with Map, Set, Register, and Text CRDTs.

use diamond_types::{
    AgentId, CRDTKind, CreateValue, OpLog, Primitive, SerializedOps, SerializedOpsOwned,
    ROOT_CRDT_ID, LV,
};
use diamond_types::list::operation::TextOperation;
use smartstring::alias::String as SmartString;

use crate::{BlockContentSnapshot, BlockId, CrdtError, Result};

/// Block document backed by unified diamond-types OpLog.
///
/// # Document Structure
///
/// ```text
/// ROOT (Map)
/// ├── blocks (Set<string>)        - OR-Set of block ID keys
/// ├── order:<id> -> f64           - Fractional index for ordering
/// └── block:<id> (Map)            - Per-block data
///     ├── kind (string)           - "thinking", "text", "tool_use", "tool_result"
///     ├── content (Text)          - For thinking/text blocks
///     ├── collapsed (bool)        - For thinking blocks
///     ├── author (string)
///     ├── created_at (i64)
///     └── (type-specific fields)
/// ```
///
/// # Convergence
///
/// All operations go through the unified OpLog, ensuring:
/// - Maps use LWW (Last-Write-Wins) semantics
/// - Sets use OR-Set (add-wins) semantics
/// - Text uses sequence CRDT for character-level merging
/// - All peers converge to identical state after sync
pub struct BlockDocument {
    /// Cell ID this document belongs to.
    cell_id: String,

    /// Agent ID string for this instance.
    agent_id_str: String,

    /// Agent ID (numeric) in the OpLog.
    agent: AgentId,

    /// Unified operation log for all CRDT operations.
    pub oplog: OpLog,

    /// LV (Local Version) of the blocks Set CRDT.
    blocks_set_lv: LV,

    /// Next sequence number for block IDs (agent-local).
    next_seq: u64,

    /// Document version (incremented on each local operation).
    version: u64,
}

impl BlockDocument {
    /// Create a new empty document.
    pub fn new(cell_id: impl Into<String>, agent_id: impl Into<String>) -> Self {
        let cell_id = cell_id.into();
        let agent_id_str = agent_id.into();

        let mut oplog = OpLog::new();
        let agent = oplog.cg.get_or_create_agent_id(&agent_id_str);

        // Create the blocks Set at ROOT_CRDT_ID["blocks"]
        let blocks_set_lv = oplog.local_map_set(
            agent,
            ROOT_CRDT_ID,
            "blocks",
            CreateValue::NewCRDT(CRDTKind::Set),
        );

        Self {
            cell_id,
            agent_id_str,
            agent,
            oplog,
            blocks_set_lv,
            next_seq: 0,
            version: 0,
        }
    }

    // =========================================================================
    // Accessors
    // =========================================================================

    /// Get the cell ID.
    pub fn cell_id(&self) -> &str {
        &self.cell_id
    }

    /// Get the agent ID.
    pub fn agent_id(&self) -> &str {
        &self.agent_id_str
    }

    /// Get the current version.
    pub fn version(&self) -> u64 {
        self.version
    }

    /// Get the number of blocks.
    pub fn block_count(&self) -> usize {
        self.oplog.checkout_set(self.blocks_set_lv).len()
    }

    /// Check if the document is empty.
    pub fn is_empty(&self) -> bool {
        self.block_count() == 0
    }

    /// Get block IDs in document order.
    fn block_ids_ordered(&self) -> Vec<BlockId> {
        // Get all block IDs from the Set
        let block_keys = self.oplog.checkout_set(self.blocks_set_lv);

        // Collect (order_value, block_id) pairs
        let mut ordered: Vec<(f64, BlockId)> = block_keys
            .iter()
            .filter_map(|p| {
                if let Primitive::Str(key) = p {
                    let block_id = BlockId::from_key(key)?;
                    let order_key = format!("order:{}", key);

                    // Get order value from map
                    let checkout = self.oplog.checkout();
                    let order_val = checkout.get(&SmartString::from(order_key.as_str()))
                        .and_then(|v| {
                            if let diamond_types::DTValue::Primitive(Primitive::I64(n)) = v.as_ref() {
                                Some(*n as f64 / 1_000_000.0)
                            } else {
                                None
                            }
                        })
                        .unwrap_or(0.0);

                    Some((order_val, block_id))
                } else {
                    None
                }
            })
            .collect();

        // Sort by order value
        ordered.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

        ordered.into_iter().map(|(_, id)| id).collect()
    }

    /// Get blocks in document order.
    pub fn blocks_ordered(&self) -> Vec<BlockSnapshot> {
        self.block_ids_ordered()
            .into_iter()
            .filter_map(|id| self.get_block_snapshot(&id))
            .collect()
    }

    /// Get a block snapshot by ID.
    pub fn get_block_snapshot(&self, id: &BlockId) -> Option<BlockSnapshot> {
        let block_key = format!("block:{}", id.to_key());
        let checkout = self.oplog.checkout();

        let block_map = checkout.get(&SmartString::from(block_key.as_str()))?;

        if let diamond_types::DTValue::Map(map) = block_map.as_ref() {
            // Extract fields from the block map
            let kind_str = map.get(&SmartString::from("kind"))
                .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::Str(s)) = v.as_ref() { Some(s.as_str()) } else { None })
                .unwrap_or("text");

            let author = map.get(&SmartString::from("author"))
                .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::Str(s)) = v.as_ref() { Some(s.to_string()) } else { None })
                .unwrap_or_default();

            let created_at = map.get(&SmartString::from("created_at"))
                .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::I64(n)) = v.as_ref() { Some(*n as u64) } else { None })
                .unwrap_or(0);

            let content = match kind_str {
                "thinking" => {
                    let text = map.get(&SmartString::from("content"))
                        .and_then(|v| if let diamond_types::DTValue::Text(s) = v.as_ref() { Some(s.clone()) } else { None })
                        .unwrap_or_default();
                    let collapsed = map.get(&SmartString::from("collapsed"))
                        .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::Bool(b)) = v.as_ref() { Some(*b) } else { None })
                        .unwrap_or(false);
                    BlockContentSnapshot::Thinking { text, collapsed }
                }
                "text" => {
                    let text = map.get(&SmartString::from("content"))
                        .and_then(|v| if let diamond_types::DTValue::Text(s) = v.as_ref() { Some(s.clone()) } else { None })
                        .unwrap_or_default();
                    BlockContentSnapshot::Text { text }
                }
                "tool_use" => {
                    let tool_id = map.get(&SmartString::from("tool_id"))
                        .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::Str(s)) = v.as_ref() { Some(s.to_string()) } else { None })
                        .unwrap_or_default();
                    let name = map.get(&SmartString::from("name"))
                        .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::Str(s)) = v.as_ref() { Some(s.to_string()) } else { None })
                        .unwrap_or_default();
                    // Read input from Text CRDT, parse as JSON
                    let input_text = map.get(&SmartString::from("content"))
                        .and_then(|v| if let diamond_types::DTValue::Text(s) = v.as_ref() { Some(s.clone()) } else { None })
                        .unwrap_or_default();
                    let input = if input_text.is_empty() {
                        serde_json::Value::Null
                    } else {
                        serde_json::from_str(&input_text).unwrap_or(serde_json::Value::Null)
                    };
                    BlockContentSnapshot::ToolUse { id: tool_id, name, input }
                }
                "tool_result" => {
                    let tool_use_id = map.get(&SmartString::from("tool_use_id"))
                        .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::Str(s)) = v.as_ref() { Some(s.to_string()) } else { None })
                        .unwrap_or_default();
                    // Read content from Text CRDT
                    let content = map.get(&SmartString::from("content"))
                        .and_then(|v| if let diamond_types::DTValue::Text(s) = v.as_ref() { Some(s.clone()) } else { None })
                        .unwrap_or_default();
                    let is_error = map.get(&SmartString::from("is_error"))
                        .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::Bool(b)) = v.as_ref() { Some(*b) } else { None })
                        .unwrap_or(false);
                    BlockContentSnapshot::ToolResult { tool_use_id, content, is_error }
                }
                _ => BlockContentSnapshot::Text { text: String::new() }
            };

            Some(BlockSnapshot {
                id: id.clone(),
                content,
                author,
                created_at,
            })
        } else {
            None
        }
    }

    /// Get full text content (concatenation of all blocks).
    pub fn full_text(&self) -> String {
        self.blocks_ordered()
            .into_iter()
            .map(|b| b.content.text().to_string())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    // =========================================================================
    // Block Operations
    // =========================================================================

    /// Generate a new block ID.
    fn new_block_id(&mut self) -> BlockId {
        let id = BlockId::new(&self.cell_id, &self.agent_id_str, self.next_seq);
        self.next_seq += 1;
        id
    }

    /// Calculate fractional index for insertion.
    fn calc_order_index(&self, after: Option<&BlockId>) -> f64 {
        let ordered = self.block_ids_ordered();

        match after {
            None => {
                // Insert at beginning
                if ordered.is_empty() {
                    1.0
                } else {
                    // Get order of first block and go before it
                    let first_key = ordered[0].to_key();
                    let order_key = format!("order:{}", first_key);
                    let checkout = self.oplog.checkout();
                    let first_order = checkout.get(&SmartString::from(order_key.as_str()))
                        .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::I64(n)) = v.as_ref() { Some(*n as f64 / 1_000_000.0) } else { None })
                        .unwrap_or(1.0);
                    first_order / 2.0
                }
            }
            Some(after_id) => {
                // Find position of after_id
                let after_idx = ordered.iter().position(|id| id == after_id);
                match after_idx {
                    Some(idx) => {
                        let after_key = ordered[idx].to_key();
                        let order_key = format!("order:{}", after_key);
                        let checkout = self.oplog.checkout();
                        let after_order = checkout.get(&SmartString::from(order_key.as_str()))
                            .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::I64(n)) = v.as_ref() { Some(*n as f64 / 1_000_000.0) } else { None })
                            .unwrap_or(1.0);

                        if idx + 1 < ordered.len() {
                            // There's a next block
                            let next_key = ordered[idx + 1].to_key();
                            let next_order_key = format!("order:{}", next_key);
                            let next_order = checkout.get(&SmartString::from(next_order_key.as_str()))
                                .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::I64(n)) = v.as_ref() { Some(*n as f64 / 1_000_000.0) } else { None })
                                .unwrap_or(after_order + 2.0);
                            (after_order + next_order) / 2.0
                        } else {
                            // Insert at end
                            after_order + 1.0
                        }
                    }
                    None => {
                        // after_id not found, insert at end
                        if let Some(last) = ordered.last() {
                            let last_key = last.to_key();
                            let order_key = format!("order:{}", last_key);
                            let checkout = self.oplog.checkout();
                            let last_order = checkout.get(&SmartString::from(order_key.as_str()))
                                .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::I64(n)) = v.as_ref() { Some(*n as f64 / 1_000_000.0) } else { None })
                                .unwrap_or(1.0);
                            last_order + 1.0
                        } else {
                            1.0
                        }
                    }
                }
            }
        }
    }

    /// Insert a text block.
    pub fn insert_text_block(
        &mut self,
        after: Option<&BlockId>,
        text: impl Into<String>,
    ) -> Result<BlockId> {
        self.insert_text_block_with_author(after, text, self.agent_id_str.clone())
    }

    /// Insert a text block with a specific author.
    pub fn insert_text_block_with_author(
        &mut self,
        after: Option<&BlockId>,
        text: impl Into<String>,
        author: impl Into<String>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let content = BlockContentSnapshot::Text { text: text.into() };
        self.insert_block_internal(id.clone(), after, content, author.into())?;
        Ok(id)
    }

    /// Insert a thinking block.
    pub fn insert_thinking_block(
        &mut self,
        after: Option<&BlockId>,
        text: impl Into<String>,
    ) -> Result<BlockId> {
        self.insert_thinking_block_with_author(after, text, self.agent_id_str.clone())
    }

    /// Insert a thinking block with a specific author.
    pub fn insert_thinking_block_with_author(
        &mut self,
        after: Option<&BlockId>,
        text: impl Into<String>,
        author: impl Into<String>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let content = BlockContentSnapshot::Thinking {
            text: text.into(),
            collapsed: false,
        };
        self.insert_block_internal(id.clone(), after, content, author.into())?;
        Ok(id)
    }

    /// Insert a tool use block.
    pub fn insert_tool_use(
        &mut self,
        after: Option<&BlockId>,
        tool_id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
    ) -> Result<BlockId> {
        self.insert_tool_use_with_author(after, tool_id, name, input, self.agent_id_str.clone())
    }

    /// Insert a tool use block with a specific author.
    pub fn insert_tool_use_with_author(
        &mut self,
        after: Option<&BlockId>,
        tool_id: impl Into<String>,
        name: impl Into<String>,
        input: serde_json::Value,
        author: impl Into<String>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let content = BlockContentSnapshot::ToolUse {
            id: tool_id.into(),
            name: name.into(),
            input,
        };
        self.insert_block_internal(id.clone(), after, content, author.into())?;
        Ok(id)
    }

    /// Insert a tool result block.
    pub fn insert_tool_result(
        &mut self,
        after: Option<&BlockId>,
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
    ) -> Result<BlockId> {
        self.insert_tool_result_with_author(after, tool_use_id, content, is_error, self.agent_id_str.clone())
    }

    /// Insert a tool result block with a specific author.
    pub fn insert_tool_result_with_author(
        &mut self,
        after: Option<&BlockId>,
        tool_use_id: impl Into<String>,
        content: impl Into<String>,
        is_error: bool,
        author: impl Into<String>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let snapshot = BlockContentSnapshot::ToolResult {
            tool_use_id: tool_use_id.into(),
            content: content.into(),
            is_error,
        };
        self.insert_block_internal(id.clone(), after, snapshot, author.into())?;
        Ok(id)
    }

    /// Internal block insertion.
    fn insert_block_internal(
        &mut self,
        id: BlockId,
        after: Option<&BlockId>,
        snapshot: BlockContentSnapshot,
        author: String,
    ) -> Result<()> {
        let block_key = id.to_key();

        // Check for duplicate
        let existing = self.oplog.checkout_set(self.blocks_set_lv);
        if existing.contains(&Primitive::Str(SmartString::from(block_key.as_str()))) {
            return Err(CrdtError::DuplicateBlock(id));
        }

        // Validate reference if provided
        if let Some(after_id) = after {
            let after_key = Primitive::Str(SmartString::from(after_id.to_key().as_str()));
            if !existing.contains(&after_key) {
                return Err(CrdtError::InvalidReference(after_id.clone()));
            }
        }

        // Calculate order index
        let order_index = self.calc_order_index(after);

        // Add block ID to the blocks Set
        self.oplog.local_set_add(
            self.agent,
            self.blocks_set_lv,
            Primitive::Str(SmartString::from(block_key.as_str())),
        );

        // Store order index (as i64 scaled by 1M for precision)
        let order_key = format!("order:{}", block_key);
        self.oplog.local_map_set(
            self.agent,
            ROOT_CRDT_ID,
            &order_key,
            CreateValue::Primitive(Primitive::I64((order_index * 1_000_000.0) as i64)),
        );

        // Create block map
        let block_map_key = format!("block:{}", block_key);
        let block_map_lv = self.oplog.local_map_set(
            self.agent,
            ROOT_CRDT_ID,
            &block_map_key,
            CreateValue::NewCRDT(CRDTKind::Map),
        );

        // Get timestamp
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        // Store block metadata
        self.oplog.local_map_set(
            self.agent,
            block_map_lv,
            "author",
            CreateValue::Primitive(Primitive::Str(SmartString::from(author.as_str()))),
        );
        self.oplog.local_map_set(
            self.agent,
            block_map_lv,
            "created_at",
            CreateValue::Primitive(Primitive::I64(created_at)),
        );

        // Store content based on type
        match &snapshot {
            BlockContentSnapshot::Thinking { text, collapsed } => {
                self.oplog.local_map_set(
                    self.agent,
                    block_map_lv,
                    "kind",
                    CreateValue::Primitive(Primitive::Str("thinking".into())),
                );
                // Create text CRDT for content
                let text_lv = self.oplog.local_map_set(
                    self.agent,
                    block_map_lv,
                    "content",
                    CreateValue::NewCRDT(CRDTKind::Text),
                );
                if !text.is_empty() {
                    self.oplog.local_text_op(self.agent, text_lv, TextOperation::new_insert(0, text));
                }
                self.oplog.local_map_set(
                    self.agent,
                    block_map_lv,
                    "collapsed",
                    CreateValue::Primitive(Primitive::Bool(*collapsed)),
                );
            }
            BlockContentSnapshot::Text { text } => {
                self.oplog.local_map_set(
                    self.agent,
                    block_map_lv,
                    "kind",
                    CreateValue::Primitive(Primitive::Str("text".into())),
                );
                let text_lv = self.oplog.local_map_set(
                    self.agent,
                    block_map_lv,
                    "content",
                    CreateValue::NewCRDT(CRDTKind::Text),
                );
                if !text.is_empty() {
                    self.oplog.local_text_op(self.agent, text_lv, TextOperation::new_insert(0, text));
                }
            }
            BlockContentSnapshot::ToolUse { id: tool_id, name, input } => {
                self.oplog.local_map_set(
                    self.agent,
                    block_map_lv,
                    "kind",
                    CreateValue::Primitive(Primitive::Str("tool_use".into())),
                );
                self.oplog.local_map_set(
                    self.agent,
                    block_map_lv,
                    "tool_id",
                    CreateValue::Primitive(Primitive::Str(SmartString::from(tool_id.as_str()))),
                );
                self.oplog.local_map_set(
                    self.agent,
                    block_map_lv,
                    "name",
                    CreateValue::Primitive(Primitive::Str(SmartString::from(name.as_str()))),
                );
                // Store input as Text CRDT for streaming support
                let input_json = serde_json::to_string(input).unwrap_or_default();
                let text_lv = self.oplog.local_map_set(
                    self.agent,
                    block_map_lv,
                    "content",
                    CreateValue::NewCRDT(CRDTKind::Text),
                );
                if !input_json.is_empty() {
                    self.oplog.local_text_op(self.agent, text_lv, TextOperation::new_insert(0, &input_json));
                }
            }
            BlockContentSnapshot::ToolResult { tool_use_id, content, is_error } => {
                self.oplog.local_map_set(
                    self.agent,
                    block_map_lv,
                    "kind",
                    CreateValue::Primitive(Primitive::Str("tool_result".into())),
                );
                self.oplog.local_map_set(
                    self.agent,
                    block_map_lv,
                    "tool_use_id",
                    CreateValue::Primitive(Primitive::Str(SmartString::from(tool_use_id.as_str()))),
                );
                // Store content as Text CRDT for streaming support
                let text_lv = self.oplog.local_map_set(
                    self.agent,
                    block_map_lv,
                    "content",
                    CreateValue::NewCRDT(CRDTKind::Text),
                );
                if !content.is_empty() {
                    self.oplog.local_text_op(self.agent, text_lv, TextOperation::new_insert(0, content));
                }
                self.oplog.local_map_set(
                    self.agent,
                    block_map_lv,
                    "is_error",
                    CreateValue::Primitive(Primitive::Bool(*is_error)),
                );
            }
        }

        self.version += 1;
        Ok(())
    }

    /// Delete a block.
    pub fn delete_block(&mut self, id: &BlockId) -> Result<()> {
        let block_key = id.to_key();
        let key_primitive = Primitive::Str(SmartString::from(block_key.as_str()));

        // Check if block exists
        let existing = self.oplog.checkout_set(self.blocks_set_lv);
        if !existing.contains(&key_primitive) {
            return Err(CrdtError::BlockNotFound(id.clone()));
        }

        // Remove from blocks Set
        self.oplog.local_set_remove(self.agent, self.blocks_set_lv, key_primitive);

        // Note: The block map and order entries remain in the oplog but are
        // effectively orphaned. This is fine for CRDT semantics.

        self.version += 1;
        Ok(())
    }

    // =========================================================================
    // Text Operations
    // =========================================================================

    /// Get the text CRDT LV for a block (if it has one).
    fn get_block_text_lv(&self, id: &BlockId) -> Option<LV> {
        let block_key = format!("block:{}", id.to_key());

        // Navigate to block:id/content
        let path = [block_key.as_str(), "content"];

        // Check if this path exists and is a Text CRDT
        let (kind, lv) = self.oplog.crdt_at_path(&path);
        if kind == CRDTKind::Text {
            Some(lv)
        } else {
            None
        }
    }

    /// Edit text within a block.
    ///
    /// All block types support text editing via their `content` Text CRDT:
    /// - Thinking/Text: Direct text content
    /// - ToolUse: JSON input as text (enables streaming)
    /// - ToolResult: Result content as text (enables streaming)
    pub fn edit_text(
        &mut self,
        id: &BlockId,
        pos: usize,
        insert: &str,
        delete: usize,
    ) -> Result<()> {
        // Verify block exists
        let _snapshot = self.get_block_snapshot(id)
            .ok_or_else(|| CrdtError::BlockNotFound(id.clone()))?;

        let text_lv = self.get_block_text_lv(id)
            .ok_or_else(|| CrdtError::Internal(format!("block {} has no text CRDT", id)))?;

        // Get current length
        let current_text = self.oplog.checkout_text(text_lv);
        let len = current_text.len_chars();

        // Validate position
        if pos > len {
            return Err(CrdtError::PositionOutOfBounds { pos, len });
        }
        if pos + delete > len {
            return Err(CrdtError::PositionOutOfBounds {
                pos: pos + delete,
                len,
            });
        }

        // Apply operations
        if delete > 0 {
            self.oplog.local_text_op(self.agent, text_lv, TextOperation::new_delete(pos..pos + delete));
        }
        if !insert.is_empty() {
            self.oplog.local_text_op(self.agent, text_lv, TextOperation::new_insert(pos, insert));
        }

        self.version += 1;
        Ok(())
    }

    /// Append text to a block.
    pub fn append_text(&mut self, id: &BlockId, text: &str) -> Result<()> {
        let snapshot = self.get_block_snapshot(id)
            .ok_or_else(|| CrdtError::BlockNotFound(id.clone()))?;

        let len = snapshot.content.text().len();
        self.edit_text(id, len, text, 0)
    }

    /// Set collapsed state of a thinking block.
    ///
    /// Only Thinking blocks support the collapsed state.
    pub fn set_collapsed(&mut self, id: &BlockId, collapsed: bool) -> Result<()> {
        let snapshot = self.get_block_snapshot(id)
            .ok_or_else(|| CrdtError::BlockNotFound(id.clone()))?;

        if !matches!(snapshot.content, BlockContentSnapshot::Thinking { .. }) {
            return Err(CrdtError::UnsupportedOperation(id.clone()));
        }

        let block_key = format!("block:{}", id.to_key());
        let (_, block_map_lv) = self.oplog.crdt_at_path(&[block_key.as_str()]);

        self.oplog.local_map_set(
            self.agent,
            block_map_lv,
            "collapsed",
            CreateValue::Primitive(Primitive::Bool(collapsed)),
        );

        self.version += 1;
        Ok(())
    }

    // =========================================================================
    // Sync Operations
    // =========================================================================

    /// Get operations since a frontier for replication.
    pub fn ops_since(&self, frontier: &[LV]) -> SerializedOpsOwned {
        self.oplog.ops_since(frontier).into()
    }

    /// Merge remote operations.
    ///
    /// Accepts borrowed `SerializedOps` to avoid lifetime conversion issues.
    /// Use `ops_since()` to get operations, and pass them directly here.
    pub fn merge_ops(&mut self, ops: SerializedOps<'_>) -> Result<()> {
        self.oplog.merge_ops(ops)
            .map_err(|e| CrdtError::Internal(format!("merge error: {:?}", e)))?;

        // Update next_seq if needed
        let blocks = self.oplog.checkout_set(self.blocks_set_lv);
        for p in blocks.iter() {
            if let Primitive::Str(key) = p
                && let Some(block_id) = BlockId::from_key(key)
                    && block_id.agent_id == self.agent_id_str {
                        self.next_seq = self.next_seq.max(block_id.seq + 1);
                    }
        }

        self.version += 1;
        Ok(())
    }

    /// Get the current frontier (version) for sync.
    pub fn frontier(&self) -> Vec<LV> {
        self.oplog.cg.version.as_ref().to_vec()
    }

    // =========================================================================
    // Serialization
    // =========================================================================

    /// Create a snapshot of the entire document.
    pub fn snapshot(&self) -> DocumentSnapshot {
        DocumentSnapshot {
            cell_id: self.cell_id.clone(),
            blocks: self.blocks_ordered(),
            version: self.version,
        }
    }

    /// Restore from a snapshot.
    pub fn from_snapshot(snapshot: DocumentSnapshot, agent_id: impl Into<String>) -> Self {
        let agent_id = agent_id.into();
        let mut doc = Self::new(&snapshot.cell_id, &agent_id);

        let mut last_id: Option<BlockId> = None;
        for block_snap in snapshot.blocks {
            let _ = doc.insert_block_internal(
                block_snap.id.clone(),
                last_id.as_ref(),
                block_snap.content,
                block_snap.author,
            );
            last_id = Some(block_snap.id);
        }

        doc.version = snapshot.version;
        doc
    }
}

/// Snapshot of a block document (serializable).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DocumentSnapshot {
    /// Cell ID.
    pub cell_id: String,
    /// Blocks in order.
    pub blocks: Vec<BlockSnapshot>,
    /// Version.
    pub version: u64,
}

/// Snapshot of a single block.
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct BlockSnapshot {
    /// Block ID.
    pub id: BlockId,
    /// Content snapshot.
    pub content: BlockContentSnapshot,
    /// Author who created this block.
    pub author: String,
    /// Timestamp when block was created (Unix millis).
    pub created_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_document() {
        let doc = BlockDocument::new("cell-1", "alice");
        assert_eq!(doc.cell_id(), "cell-1");
        assert_eq!(doc.agent_id(), "alice");
        assert!(doc.is_empty());
        assert_eq!(doc.version(), 0);
    }

    #[test]
    fn test_insert_and_order() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        let id1 = doc.insert_text_block(None, "First").unwrap();
        let id2 = doc.insert_text_block(Some(&id1), "Second").unwrap();
        let id3 = doc.insert_text_block(Some(&id2), "Third").unwrap();

        let order: Vec<_> = doc.blocks_ordered().iter().map(|b| b.id.clone()).collect();
        assert_eq!(order, vec![id1, id2, id3]);
    }

    #[test]
    fn test_insert_at_beginning() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        let id1 = doc.insert_text_block(None, "First").unwrap();
        let id2 = doc.insert_text_block(None, "Before First").unwrap();

        let order: Vec<_> = doc.blocks_ordered().iter().map(|b| b.id.clone()).collect();
        assert_eq!(order, vec![id2, id1]);
    }

    #[test]
    fn test_snapshot_roundtrip() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        doc.insert_thinking_block(None, "Thinking...").unwrap();
        doc.insert_text_block(None, "Response").unwrap();

        let snapshot = doc.snapshot();
        let restored = BlockDocument::from_snapshot(snapshot.clone(), "bob");

        assert_eq!(restored.block_count(), doc.block_count());
        assert_eq!(restored.full_text(), doc.full_text());
    }

    #[test]
    fn test_text_editing() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        let id = doc.insert_text_block(None, "Hello").unwrap();
        doc.append_text(&id, " World").unwrap();

        let text = doc.get_block_snapshot(&id).unwrap().content.text().to_string();
        assert_eq!(text, "Hello World");

        doc.edit_text(&id, 5, ",", 0).unwrap();
        let text = doc.get_block_snapshot(&id).unwrap().content.text().to_string();
        assert_eq!(text, "Hello, World");
    }

    #[test]
    fn test_tool_use_block_editable() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        // Create tool use with JSON input
        let tool_id = doc.insert_tool_use(None, "tool-1", "read_file", serde_json::json!({"path": "/foo"})).unwrap();

        // Verify initial content
        let snapshot = doc.get_block_snapshot(&tool_id).unwrap();
        if let BlockContentSnapshot::ToolUse { input, .. } = &snapshot.content {
            assert_eq!(input["path"], "/foo");
        } else {
            panic!("Expected ToolUse");
        }

        // Tool use content IS now editable (for streaming)
        // Note: This appends to the JSON string, potentially making it invalid JSON
        // In practice, streaming would build up valid JSON incrementally
        let result = doc.edit_text(&tool_id, 0, "", 0); // No-op edit
        assert!(result.is_ok());
    }

    #[test]
    fn test_tool_result_streaming() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        // Create empty tool result (simulating streaming start)
        let result_id = doc.insert_tool_result(None, "tool-1", "", false).unwrap();

        // Stream content incrementally
        doc.append_text(&result_id, "Line 1\n").unwrap();
        doc.append_text(&result_id, "Line 2\n").unwrap();
        doc.append_text(&result_id, "Line 3").unwrap();

        // Verify streamed content
        let snapshot = doc.get_block_snapshot(&result_id).unwrap();
        if let BlockContentSnapshot::ToolResult { content, .. } = &snapshot.content {
            assert_eq!(content, "Line 1\nLine 2\nLine 3");
        } else {
            panic!("Expected ToolResult");
        }
    }
}
