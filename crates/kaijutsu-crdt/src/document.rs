//! Block document with unified CRDT using diamond-types.
//!
//! Uses the diamond-types fork (github.com/tobert/diamond-types, branch feat/maps-and-uuids)
//! which provides unified OpLog with Map, Set, Register, and Text CRDTs.
//!
//! # DAG Model
//!
//! Blocks form a DAG via parent_id links. Each block can have:
//! - A parent block (tool results under tool calls, responses under prompts)
//! - Multiple children (parallel tool calls, multiple response blocks)
//! - Role and status for conversation flow tracking

use diamond_types::{
    AgentId, CRDTKind, CreateValue, OpLog, Primitive, SerializedOps, SerializedOpsOwned,
    ROOT_CRDT_ID, LV,
};
use diamond_types::list::operation::TextOperation;
use smartstring::alias::String as SmartString;

use crate::{BlockId, BlockKind, BlockSnapshot, CrdtError, Result, Role, Status};

/// Block document backed by unified diamond-types OpLog.
///
/// # Document Structure
///
/// ```text
/// ROOT (Map)
/// ├── blocks (Set<string>)          # OR-Set of block ID keys
/// ├── order:<id> -> f64             # Fractional index for ordering
/// └── block:<id> (Map)              # Per-block data
///     ├── parent_id (Str | null)    # DAG edge - write once
///     ├── role (Str)                # human/agent/system/tool
///     ├── status (Str)              # pending/running/done/error (LWW)
///     ├── kind (Str)                # text/thinking/tool_call/tool_result
///     ├── content (Text)            # Streamable via Text CRDT
///     ├── collapsed (Bool)          # LWW - toggleable
///     ├── author (Str)
///     ├── created_at (I64)
///     │
///     │   # Tool-specific (optional, set on creation)
///     ├── tool_name (Str)           # For ToolCall
///     ├── tool_input (Text)         # For ToolCall - JSON, streamable
///     ├── tool_call_id (Str)        # For ToolResult → parent ToolCall
///     ├── exit_code (I64)           # For ToolResult
///     └── is_error (Bool)           # For ToolResult
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
    /// Create a new empty document (server-side, creates initial structure).
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

    /// Create an empty document for sync (client-side, no initial operations).
    ///
    /// Use this when the document will receive its initial state via `merge_ops`.
    /// The blocks Set will be created when ops are merged from the server.
    pub fn new_for_sync(cell_id: impl Into<String>, agent_id: impl Into<String>) -> Self {
        let cell_id = cell_id.into();
        let agent_id_str = agent_id.into();

        let oplog = OpLog::new();
        // Don't create blocks Set - it will come from merged ops
        // blocks_set_lv will be looked up after first merge

        Self {
            cell_id,
            agent_id_str,
            agent: 0, // Will be set on first local operation
            oplog,
            blocks_set_lv: 0, // Invalid until ops are merged
            next_seq: 0,
            version: 0,
        }
    }

    // NOTE: refresh_after_merge was removed - we need a proper sync protocol
    // where the server sends the full oplog on initial connect.

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
            let kind = BlockKind::from_str(kind_str).unwrap_or_default();

            let role_str = map.get(&SmartString::from("role"))
                .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::Str(s)) = v.as_ref() { Some(s.as_str()) } else { None })
                .unwrap_or("human");
            let role = Role::from_str(role_str).unwrap_or_default();

            let status_str = map.get(&SmartString::from("status"))
                .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::Str(s)) = v.as_ref() { Some(s.as_str()) } else { None })
                .unwrap_or("done");
            let status = Status::from_str(status_str).unwrap_or(Status::Done);

            let author = map.get(&SmartString::from("author"))
                .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::Str(s)) = v.as_ref() { Some(s.to_string()) } else { None })
                .unwrap_or_default();

            let created_at = map.get(&SmartString::from("created_at"))
                .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::I64(n)) = v.as_ref() { Some(*n as u64) } else { None })
                .unwrap_or(0);

            let content = map.get(&SmartString::from("content"))
                .and_then(|v| if let diamond_types::DTValue::Text(s) = v.as_ref() { Some(s.clone()) } else { None })
                .unwrap_or_default();

            let collapsed = map.get(&SmartString::from("collapsed"))
                .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::Bool(b)) = v.as_ref() { Some(*b) } else { None })
                .unwrap_or(false);

            let parent_id = map.get(&SmartString::from("parent_id"))
                .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::Str(s)) = v.as_ref() { BlockId::from_key(s) } else { None });

            // Tool-specific fields
            let tool_name = map.get(&SmartString::from("tool_name"))
                .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::Str(s)) = v.as_ref() { Some(s.to_string()) } else { None });

            let tool_input = map.get(&SmartString::from("tool_input"))
                .and_then(|v| if let diamond_types::DTValue::Text(s) = v.as_ref() { serde_json::from_str(s).ok() } else { None });

            let tool_call_id = map.get(&SmartString::from("tool_call_id"))
                .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::Str(s)) = v.as_ref() { BlockId::from_key(s) } else { None });

            let exit_code = map.get(&SmartString::from("exit_code"))
                .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::I64(n)) = v.as_ref() { Some(*n as i32) } else { None });

            let is_error = map.get(&SmartString::from("is_error"))
                .and_then(|v| if let diamond_types::DTValue::Primitive(Primitive::Bool(b)) = v.as_ref() { Some(*b) } else { None })
                .unwrap_or(false);

            Some(BlockSnapshot {
                id: id.clone(),
                parent_id,
                role,
                status,
                kind,
                content,
                collapsed,
                author,
                created_at,
                tool_name,
                tool_input,
                tool_call_id,
                exit_code,
                is_error,
            })
        } else {
            None
        }
    }

    /// Get full text content (concatenation of all blocks).
    pub fn full_text(&self) -> String {
        self.blocks_ordered()
            .into_iter()
            .map(|b| b.content.clone())
            .collect::<Vec<_>>()
            .join("\n\n")
    }

    // =========================================================================
    // DAG Operations
    // =========================================================================

    /// Get children of a block (blocks with this block as parent).
    pub fn get_children(&self, parent_id: &BlockId) -> Vec<BlockId> {
        self.blocks_ordered()
            .into_iter()
            .filter(|b| b.parent_id.as_ref() == Some(parent_id))
            .map(|b| b.id)
            .collect()
    }

    /// Get ancestors of a block (walk up the parent chain).
    pub fn get_ancestors(&self, id: &BlockId) -> Vec<BlockId> {
        let mut ancestors = Vec::new();
        let mut current = self.get_block_snapshot(id);

        while let Some(block) = current {
            if let Some(parent_id) = block.parent_id {
                ancestors.push(parent_id.clone());
                current = self.get_block_snapshot(&parent_id);
            } else {
                break;
            }
        }

        ancestors
    }

    /// Get root blocks (blocks with no parent).
    pub fn get_roots(&self) -> Vec<BlockId> {
        self.blocks_ordered()
            .into_iter()
            .filter(|b| b.parent_id.is_none())
            .map(|b| b.id)
            .collect()
    }

    /// Get the depth of a block in the DAG (0 for roots).
    pub fn get_depth(&self, id: &BlockId) -> usize {
        self.get_ancestors(id).len()
    }

    // =========================================================================
    // Block Operations - New DAG-native API
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

    /// Insert a new block with full DAG support.
    ///
    /// This is the primary block creation API. All legacy insert_* methods
    /// delegate to this.
    ///
    /// # Arguments
    ///
    /// * `parent_id` - Parent block ID for DAG relationship (None for root)
    /// * `after` - Block ID to insert after in document order (None for beginning)
    /// * `role` - Role of the block author (Human, Agent, System, Tool)
    /// * `kind` - Content type (Text, Thinking, ToolCall, ToolResult)
    /// * `content` - Initial text content
    /// * `author` - Author identifier
    ///
    /// # Returns
    ///
    /// The new block's ID on success.
    pub fn insert_block(
        &mut self,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        role: Role,
        kind: BlockKind,
        content: impl Into<String>,
        author: impl Into<String>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let content_str = content.into();
        let author_str = author.into();

        self.insert_block_with_id(
            id.clone(),
            parent_id,
            after,
            role,
            kind,
            content_str,
            author_str,
            None, // tool_name
            None, // tool_input
            None, // tool_call_id
            None, // exit_code
            false, // is_error
        )?;

        Ok(id)
    }

    /// Insert a tool call block.
    pub fn insert_tool_call(
        &mut self,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        tool_name: impl Into<String>,
        tool_input: serde_json::Value,
        author: impl Into<String>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let input_json = serde_json::to_string_pretty(&tool_input).unwrap_or_default();

        self.insert_block_with_id(
            id.clone(),
            parent_id,
            after,
            Role::Model,
            BlockKind::ToolCall,
            input_json,
            author.into(),
            Some(tool_name.into()),
            Some(tool_input),
            None,
            None,
            false,
        )?;

        Ok(id)
    }

    /// Insert a tool result block.
    pub fn insert_tool_result_block(
        &mut self,
        tool_call_id: &BlockId,
        after: Option<&BlockId>,
        content: impl Into<String>,
        is_error: bool,
        exit_code: Option<i32>,
        author: impl Into<String>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();

        self.insert_block_with_id(
            id.clone(),
            Some(tool_call_id), // Parent is the tool call
            after,
            Role::Tool,
            BlockKind::ToolResult,
            content.into(),
            author.into(),
            None,
            None,
            Some(tool_call_id.clone()),
            exit_code,
            is_error,
        )?;

        Ok(id)
    }

    /// Insert a block from a complete snapshot (for remote sync).
    ///
    /// This is used when receiving blocks from the server via block events.
    /// The snapshot contains all fields including the pre-assigned block ID.
    ///
    /// # Arguments
    ///
    /// * `snapshot` - Complete block snapshot from remote source
    /// * `after` - Block ID to insert after in document order (None for end)
    ///
    /// # Returns
    ///
    /// The block's ID on success.
    pub fn insert_from_snapshot(
        &mut self,
        snapshot: BlockSnapshot,
        after: Option<&BlockId>,
    ) -> Result<BlockId> {
        // Update next_seq if needed to avoid collisions
        if snapshot.id.agent_id == self.agent_id_str {
            self.next_seq = self.next_seq.max(snapshot.id.seq + 1);
        }

        self.insert_block_with_id(
            snapshot.id.clone(),
            snapshot.parent_id.as_ref(),
            after,
            snapshot.role,
            snapshot.kind,
            snapshot.content,
            snapshot.author,
            snapshot.tool_name,
            snapshot.tool_input,
            snapshot.tool_call_id,
            snapshot.exit_code,
            snapshot.is_error,
        )?;

        Ok(snapshot.id)
    }

    /// Internal block insertion with all fields.
    #[allow(clippy::too_many_arguments)]
    fn insert_block_with_id(
        &mut self,
        id: BlockId,
        parent_id: Option<&BlockId>,
        after: Option<&BlockId>,
        role: Role,
        kind: BlockKind,
        content: String,
        author: String,
        tool_name: Option<String>,
        tool_input: Option<serde_json::Value>,
        tool_call_id: Option<BlockId>,
        exit_code: Option<i32>,
        is_error: bool,
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

        // Validate parent if provided
        if let Some(parent) = parent_id {
            let parent_key = Primitive::Str(SmartString::from(parent.to_key().as_str()));
            if !existing.contains(&parent_key) {
                return Err(CrdtError::InvalidReference(parent.clone()));
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
        self.oplog.local_map_set(
            self.agent,
            block_map_lv,
            "kind",
            CreateValue::Primitive(Primitive::Str(SmartString::from(kind.as_str()))),
        );
        self.oplog.local_map_set(
            self.agent,
            block_map_lv,
            "role",
            CreateValue::Primitive(Primitive::Str(SmartString::from(role.as_str()))),
        );
        self.oplog.local_map_set(
            self.agent,
            block_map_lv,
            "status",
            CreateValue::Primitive(Primitive::Str(SmartString::from(Status::Done.as_str()))),
        );
        self.oplog.local_map_set(
            self.agent,
            block_map_lv,
            "collapsed",
            CreateValue::Primitive(Primitive::Bool(false)),
        );

        // Store parent_id if present
        if let Some(parent) = parent_id {
            self.oplog.local_map_set(
                self.agent,
                block_map_lv,
                "parent_id",
                CreateValue::Primitive(Primitive::Str(SmartString::from(parent.to_key().as_str()))),
            );
        }

        // Create text CRDT for content
        let text_lv = self.oplog.local_map_set(
            self.agent,
            block_map_lv,
            "content",
            CreateValue::NewCRDT(CRDTKind::Text),
        );
        if !content.is_empty() {
            self.oplog.local_text_op(self.agent, text_lv, TextOperation::new_insert(0, &content));
        }

        // Tool-specific fields
        if let Some(name) = tool_name {
            self.oplog.local_map_set(
                self.agent,
                block_map_lv,
                "tool_name",
                CreateValue::Primitive(Primitive::Str(SmartString::from(name.as_str()))),
            );
        }

        if let Some(input) = tool_input {
            // Store tool_input as Text CRDT for streaming support
            let input_json = serde_json::to_string(&input).unwrap_or_default();
            let input_lv = self.oplog.local_map_set(
                self.agent,
                block_map_lv,
                "tool_input",
                CreateValue::NewCRDT(CRDTKind::Text),
            );
            if !input_json.is_empty() {
                self.oplog.local_text_op(self.agent, input_lv, TextOperation::new_insert(0, &input_json));
            }
        }

        if let Some(tcid) = tool_call_id {
            self.oplog.local_map_set(
                self.agent,
                block_map_lv,
                "tool_call_id",
                CreateValue::Primitive(Primitive::Str(SmartString::from(tcid.to_key().as_str()))),
            );
        }

        if let Some(code) = exit_code {
            self.oplog.local_map_set(
                self.agent,
                block_map_lv,
                "exit_code",
                CreateValue::Primitive(Primitive::I64(code as i64)),
            );
        }

        if is_error {
            self.oplog.local_map_set(
                self.agent,
                block_map_lv,
                "is_error",
                CreateValue::Primitive(Primitive::Bool(true)),
            );
        }

        self.version += 1;
        Ok(())
    }

    /// Set the status of a block.
    ///
    /// Status is LWW (Last-Write-Wins) for convergence.
    pub fn set_status(&mut self, id: &BlockId, status: Status) -> Result<()> {
        let _snapshot = self.get_block_snapshot(id)
            .ok_or_else(|| CrdtError::BlockNotFound(id.clone()))?;

        let block_key = format!("block:{}", id.to_key());
        let (_, block_map_lv) = self.oplog.crdt_at_path(&[block_key.as_str()]);

        self.oplog.local_map_set(
            self.agent,
            block_map_lv,
            "status",
            CreateValue::Primitive(Primitive::Str(SmartString::from(status.as_str()))),
        );

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
    /// - ToolCall: JSON input as text (enables streaming)
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

        // Use chars().count() for UTF-8 safety - edit_text uses character positions
        let len = snapshot.content.chars().count();
        self.edit_text(id, len, text, 0)
    }

    /// Set collapsed state of a thinking block.
    ///
    /// Only Thinking blocks support the collapsed state.
    pub fn set_collapsed(&mut self, id: &BlockId, collapsed: bool) -> Result<()> {
        let snapshot = self.get_block_snapshot(id)
            .ok_or_else(|| CrdtError::BlockNotFound(id.clone()))?;

        if snapshot.kind != BlockKind::Thinking {
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

    /// Move a block to a new position in the ordering.
    ///
    /// The block will be placed immediately after `after_id`, or at the
    /// beginning if `after_id` is None.
    pub fn move_block(&mut self, id: &BlockId, after: Option<&BlockId>) -> Result<()> {
        let block_key = id.to_key();
        let key_primitive = Primitive::Str(SmartString::from(block_key.as_str()));

        // Check if block exists
        let existing = self.oplog.checkout_set(self.blocks_set_lv);
        if !existing.contains(&key_primitive) {
            return Err(CrdtError::BlockNotFound(id.clone()));
        }

        // Validate reference if provided
        if let Some(after_id) = after {
            let after_key = Primitive::Str(SmartString::from(after_id.to_key().as_str()));
            if !existing.contains(&after_key) {
                return Err(CrdtError::InvalidReference(after_id.clone()));
            }
        }

        // Calculate new order index
        let order_index = self.calc_order_index(after);

        // Update order key
        let order_key = format!("order:{}", block_key);
        self.oplog.local_map_set(
            self.agent,
            ROOT_CRDT_ID,
            &order_key,
            CreateValue::Primitive(Primitive::I64((order_index * 1_000_000.0) as i64)),
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

    /// Merge remote operations (owned version for cross-thread/network use).
    ///
    /// Use this when receiving serialized ops that have been deserialized
    /// into the owned form (e.g., from network RPC).
    pub fn merge_ops_owned(&mut self, ops: SerializedOpsOwned) -> Result<()> {
        self.oplog.merge_ops_owned(ops)
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
    // Oplog-Based Sync (for initial state transfer)
    // =========================================================================

    /// Get full oplog as serialized bytes (for initial sync).
    ///
    /// This serializes the complete oplog from empty frontier, enabling clients
    /// to receive the full CRDT history including the root "blocks" Set creation.
    /// This is essential for proper sync - clients cannot merge incremental ops
    /// without having the oplog root operations.
    pub fn oplog_bytes(&self) -> Vec<u8> {
        let ops = self.ops_since(&[]); // Full oplog from empty frontier
        serde_json::to_vec(&ops).unwrap_or_default()
    }

    /// Create document from serialized oplog (client-side sync).
    ///
    /// This is the proper way to initialize a client document for sync.
    /// Instead of creating an independent oplog root (which breaks causality),
    /// we start with an empty OpLog and merge the server's full oplog.
    ///
    /// # Arguments
    ///
    /// * `cell_id` - Cell ID for the document
    /// * `agent_id` - Agent ID for local operations
    /// * `oplog_bytes` - Serialized oplog from server's `oplog_bytes()`
    ///
    /// # Returns
    ///
    /// A BlockDocument with the same oplog state as the server, ready for
    /// incremental sync via `merge_ops`.
    pub fn from_oplog(
        cell_id: impl Into<String>,
        agent_id: impl Into<String>,
        oplog_bytes: &[u8],
    ) -> Result<Self> {
        let cell_id = cell_id.into();
        let agent_id_str = agent_id.into();

        // Start with empty oplog (no independent "blocks" Set!)
        let mut oplog = OpLog::new();

        // Merge server's full oplog
        let ops: SerializedOpsOwned = serde_json::from_slice(oplog_bytes)
            .map_err(|e| CrdtError::Internal(format!("deserialize oplog: {}", e)))?;
        oplog
            .merge_ops_owned(ops)
            .map_err(|e| CrdtError::Internal(format!("merge oplog: {:?}", e)))?;

        // Find blocks_set_lv from merged oplog by looking up ROOT["blocks"]
        let (kind, blocks_set_lv) = oplog.crdt_at_path(&["blocks"]);
        if kind != CRDTKind::Set {
            return Err(CrdtError::Internal(
                "oplog missing 'blocks' Set at root".into(),
            ));
        }

        let agent = oplog.cg.get_or_create_agent_id(&agent_id_str);

        // Calculate next_seq from existing blocks (avoid ID collisions)
        let mut next_seq = 0u64;
        let blocks = oplog.checkout_set(blocks_set_lv);
        for p in blocks.iter() {
            if let Primitive::Str(key) = p {
                if let Some(block_id) = BlockId::from_key(key) {
                    if block_id.agent_id == agent_id_str {
                        next_seq = next_seq.max(block_id.seq + 1);
                    }
                }
            }
        }

        // Use block count as initial version - ensures non-zero if content exists
        // This makes sync detect the document has changed from an empty state
        let block_count = oplog.checkout_set(blocks_set_lv).len();
        let version = if block_count > 0 { block_count as u64 } else { 0 };

        Ok(Self {
            cell_id,
            agent_id_str,
            agent,
            oplog,
            blocks_set_lv,
            next_seq,
            version,
        })
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
            // Track max seq for our agent_id to avoid ID collisions
            if block_snap.id.agent_id == agent_id {
                doc.next_seq = doc.next_seq.max(block_snap.id.seq + 1);
            }

            let _ = doc.insert_block_with_id(
                block_snap.id.clone(),
                block_snap.parent_id.as_ref(),
                last_id.as_ref(),
                block_snap.role,
                block_snap.kind,
                block_snap.content,
                block_snap.author,
                block_snap.tool_name,
                block_snap.tool_input,
                block_snap.tool_call_id,
                block_snap.exit_code,
                block_snap.is_error,
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
    fn test_insert_block_new_api() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        let id1 = doc.insert_block(
            None,
            None,
            Role::User,
            BlockKind::Text,
            "Hello!",
            "user:amy",
        ).unwrap();

        let id2 = doc.insert_block(
            Some(&id1), // Parent is id1
            Some(&id1), // After id1
            Role::Model,
            BlockKind::Text,
            "Hi there!",
            "model:claude",
        ).unwrap();

        let blocks = doc.blocks_ordered();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].id, id1);
        assert_eq!(blocks[0].role, Role::User);
        assert_eq!(blocks[1].id, id2);
        assert_eq!(blocks[1].role, Role::Model);
        assert_eq!(blocks[1].parent_id, Some(id1.clone()));
    }

    #[test]
    fn test_dag_operations() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        // Create a parent block
        let parent_id = doc.insert_block(
            None,
            None,
            Role::User,
            BlockKind::Text,
            "Question",
            "user:amy",
        ).unwrap();

        // Create children
        let child1 = doc.insert_block(
            Some(&parent_id),
            Some(&parent_id),
            Role::Model,
            BlockKind::Thinking,
            "Thinking...",
            "model:claude",
        ).unwrap();

        let child2 = doc.insert_block(
            Some(&parent_id),
            Some(&child1),
            Role::Model,
            BlockKind::Text,
            "Answer",
            "model:claude",
        ).unwrap();

        // Test get_children
        let children = doc.get_children(&parent_id);
        assert_eq!(children.len(), 2);
        assert!(children.contains(&child1));
        assert!(children.contains(&child2));

        // Test get_ancestors
        let ancestors = doc.get_ancestors(&child1);
        assert_eq!(ancestors, vec![parent_id.clone()]);

        // Test get_roots
        let roots = doc.get_roots();
        assert_eq!(roots, vec![parent_id.clone()]);

        // Test get_depth
        assert_eq!(doc.get_depth(&parent_id), 0);
        assert_eq!(doc.get_depth(&child1), 1);
    }

    #[test]
    fn test_set_status() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        let id = doc.insert_block(
            None,
            None,
            Role::Model,
            BlockKind::ToolCall,
            "{}",
            "model:claude",
        ).unwrap();

        // Initially status is Done
        let snap = doc.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.status, Status::Done);

        // Set to Running
        doc.set_status(&id, Status::Running).unwrap();
        let snap = doc.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.status, Status::Running);

        // Set to Error
        doc.set_status(&id, Status::Error).unwrap();
        let snap = doc.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.status, Status::Error);
    }

    #[test]
    fn test_tool_call_and_result() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        // Create tool call
        let tool_call_id = doc.insert_tool_call(
            None,
            None,
            "read_file",
            serde_json::json!({"path": "/etc/hosts"}),
            "model:claude",
        ).unwrap();

        let snap = doc.get_block_snapshot(&tool_call_id).unwrap();
        assert_eq!(snap.kind, BlockKind::ToolCall);
        assert_eq!(snap.tool_name, Some("read_file".to_string()));

        // Create tool result
        let result_id = doc.insert_tool_result_block(
            &tool_call_id,
            Some(&tool_call_id),
            "127.0.0.1 localhost",
            false,
            Some(0),
            "system",
        ).unwrap();

        let snap = doc.get_block_snapshot(&result_id).unwrap();
        assert_eq!(snap.kind, BlockKind::ToolResult);
        assert_eq!(snap.parent_id, Some(tool_call_id.clone()));
        assert_eq!(snap.tool_call_id, Some(tool_call_id));
        assert!(!snap.is_error);
        assert_eq!(snap.exit_code, Some(0));
    }

    #[test]
    fn test_insert_and_order() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        let id1 = doc.insert_block(None, None, Role::User, BlockKind::Text, "First", "alice").unwrap();
        let id2 = doc.insert_block(None, Some(&id1), Role::User, BlockKind::Text, "Second", "alice").unwrap();
        let id3 = doc.insert_block(None, Some(&id2), Role::User, BlockKind::Text, "Third", "alice").unwrap();

        let order: Vec<_> = doc.blocks_ordered().iter().map(|b| b.id.clone()).collect();
        assert_eq!(order, vec![id1, id2, id3]);
    }

    #[test]
    fn test_insert_at_beginning() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        let id1 = doc.insert_block(None, None, Role::User, BlockKind::Text, "First", "alice").unwrap();
        let id2 = doc.insert_block(None, None, Role::User, BlockKind::Text, "Before First", "alice").unwrap();

        let order: Vec<_> = doc.blocks_ordered().iter().map(|b| b.id.clone()).collect();
        assert_eq!(order, vec![id2, id1]);
    }

    #[test]
    fn test_snapshot_roundtrip() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        doc.insert_block(None, None, Role::Model, BlockKind::Thinking, "Thinking...", "model:claude").unwrap();
        doc.insert_block(None, None, Role::Model, BlockKind::Text, "Response", "model:claude").unwrap();

        let snapshot = doc.snapshot();
        let restored = BlockDocument::from_snapshot(snapshot.clone(), "bob");

        assert_eq!(restored.block_count(), doc.block_count());
        assert_eq!(restored.full_text(), doc.full_text());
    }

    #[test]
    fn test_text_editing() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        let id = doc.insert_block(None, None, Role::User, BlockKind::Text, "Hello", "user:amy").unwrap();
        doc.append_text(&id, " World").unwrap();

        let text = doc.get_block_snapshot(&id).unwrap().content;
        assert_eq!(text, "Hello World");

        doc.edit_text(&id, 5, ",", 0).unwrap();
        let text = doc.get_block_snapshot(&id).unwrap().content;
        assert_eq!(text, "Hello, World");
    }

    #[test]
    fn test_move_block() {
        let mut doc = BlockDocument::new("cell-1", "alice");

        // Insert three blocks: A, B, C
        let a = doc.insert_block(None, None, Role::User, BlockKind::Text, "A", "user").unwrap();
        let b = doc.insert_block(None, Some(&a), Role::User, BlockKind::Text, "B", "user").unwrap();
        let c = doc.insert_block(None, Some(&b), Role::User, BlockKind::Text, "C", "user").unwrap();

        // Initial order should be A, B, C
        let ordered = doc.blocks_ordered();
        assert_eq!(ordered.len(), 3);
        assert_eq!(ordered[0].id, a);
        assert_eq!(ordered[1].id, b);
        assert_eq!(ordered[2].id, c);

        // Move C to the beginning (before A)
        doc.move_block(&c, None).unwrap();
        let ordered = doc.blocks_ordered();
        assert_eq!(ordered[0].id, c, "C should be first after moving to beginning");
        assert_eq!(ordered[1].id, a);
        assert_eq!(ordered[2].id, b);

        // Move A after B (to the end)
        doc.move_block(&a, Some(&b)).unwrap();
        let ordered = doc.blocks_ordered();
        assert_eq!(ordered[0].id, c);
        assert_eq!(ordered[1].id, b);
        assert_eq!(ordered[2].id, a, "A should be last after moving after B");

        // Moving non-existent block should fail
        let fake_id = BlockId::new("cell-1", "fake", 999);
        assert!(doc.move_block(&fake_id, None).is_err());

        // Moving after non-existent block should fail
        assert!(doc.move_block(&a, Some(&fake_id)).is_err());
    }

    /// Test that demonstrates the client-server sync failure.
    ///
    /// The problem: Both server and client call `BlockDocument::new()`, which creates
    /// an independent "blocks" Set operation. When server sends `ops_since(frontier)`
    /// (incremental ops), those ops reference the server's "blocks" Set creation which
    /// doesn't exist in the client's oplog -> DataMissing error.
    ///
    /// This test replicates the exact behavior causing streaming failures in the UI.
    #[test]
    fn test_sync_fails_with_independent_blocks_set_creation() {
        // === Server side ===
        // Server creates its document - this creates a "blocks" Set operation
        let mut server = BlockDocument::new("cell-1", "server-agent");

        // Capture frontier AFTER "blocks" Set was created (current buggy behavior)
        let frontier_after_init = server.frontier();

        // Server inserts a block
        let _block_id = server.insert_block(
            None,
            None,
            Role::User,
            BlockKind::Text,
            "Hello from server",
            "user:alice"
        ).unwrap();

        // Server gets ops since AFTER init (only the block insert, not the "blocks" Set)
        let incremental_ops = server.ops_since(&frontier_after_init);

        // === Client side ===
        // Client creates its OWN document - this creates a DIFFERENT "blocks" Set operation
        let mut client = BlockDocument::new("cell-1", "client-agent");

        // Client tries to merge the incremental ops
        // THIS SHOULD FAIL because the ops reference server's "blocks" Set creation
        // which doesn't exist in client's oplog
        let result = client.merge_ops_owned(incremental_ops);

        // BUG DEMONSTRATION: This assert documents the current broken behavior.
        // When we fix the bug, this test should be updated to expect success.
        assert!(
            result.is_err(),
            "Expected DataMissing error when merging incremental ops with independent oplog roots"
        );

        // Verify the error is specifically about missing data
        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(
            err_msg.contains("DataMissing") || err_msg.contains("Missing"),
            "Expected DataMissing error, got: {}", err_msg
        );
    }

    /// Test that demonstrates the correct fix: sending full oplog.
    ///
    /// When server sends `ops_since(&[])` (from empty frontier), it includes
    /// the "blocks" Set creation, allowing the client to merge successfully.
    #[test]
    fn test_sync_succeeds_with_full_oplog() {
        // === Server side ===
        let mut server = BlockDocument::new("cell-1", "server-agent");

        // Server inserts a block
        let _block_id = server.insert_block(
            None,
            None,
            Role::User,
            BlockKind::Text,
            "Hello from server",
            "user:alice"
        ).unwrap();

        // Server gets ops from EMPTY frontier (full oplog, including "blocks" Set creation)
        let full_ops = server.ops_since(&[]);

        // === Client side ===
        // Client needs an empty document (no independent "blocks" Set)
        // Create a new OpLog directly to simulate a clean sync client
        let mut client_oplog = OpLog::new();

        // Merge the full ops - this should work because it includes "blocks" Set creation
        let result = client_oplog.merge_ops_owned(full_ops.clone());

        assert!(
            result.is_ok(),
            "Full oplog merge should succeed: {:?}", result
        );

        // Verify client oplog has operations (simple sanity check)
        // The full checkout should contain the "blocks" key and the block data
        let checkout = client_oplog.checkout();
        assert!(
            checkout.get(&SmartString::from("blocks")).is_some(),
            "Client should have 'blocks' key after merging full oplog"
        );
    }

    /// Test incremental sync after initial full sync.
    ///
    /// After client receives full oplog and establishes sync, subsequent
    /// incremental ops should merge correctly.
    #[test]
    fn test_incremental_sync_after_full_sync() {
        // === Initial full sync ===
        let mut server = BlockDocument::new("cell-1", "server-agent");
        let full_ops = server.ops_since(&[]);

        // Client creates empty oplog and merges full state
        let mut client_oplog = OpLog::new();
        client_oplog.merge_ops_owned(full_ops).expect("initial sync should work");

        // === Server continues ===
        let frontier_before_block = server.frontier();

        let _block_id = server.insert_block(
            None,
            None,
            Role::User,
            BlockKind::Text,
            "New block",
            "user:alice"
        ).unwrap();

        // Get incremental ops for just the block insert
        let incremental_ops = server.ops_since(&frontier_before_block);

        // Client merges incremental ops - should work now because roots match
        let result = client_oplog.merge_ops_owned(incremental_ops);

        assert!(
            result.is_ok(),
            "Incremental merge after full sync should succeed: {:?}", result
        );
    }

    /// Test oplog-based sync: client receives full oplog, then streams work.
    ///
    /// This replaces the old snapshot-based approach. The server sends its full
    /// oplog, client creates document from it, and subsequent incremental ops merge.
    #[test]
    fn test_snapshot_then_streaming_should_work() {
        // === Server: initial state ===
        let mut server = BlockDocument::new("cell-1", "server-agent");
        let block_id = server.insert_block(
            None,
            None,
            Role::Model,
            BlockKind::Text,
            "Initial content",
            "model:claude"
        ).unwrap();

        // Server sends full oplog to client (the fix!)
        let oplog_bytes = server.oplog_bytes();

        // === Client: receives full oplog ===
        // Fixed: from_oplog merges server's oplog → shared history
        let mut client = BlockDocument::from_oplog("cell-1", "client-agent", &oplog_bytes)
            .expect("from_oplog should succeed");

        // Verify client has the block
        assert_eq!(client.block_count(), 1);

        // === Server: continues streaming ===
        let frontier_before_append = server.frontier();
        server.append_text(&block_id, " more text").unwrap();
        let incremental_ops = server.ops_since(&frontier_before_append);

        // === Client: merges incremental ops ===
        // Now works because client and server share oplog history
        let result = client.merge_ops_owned(incremental_ops);

        assert!(
            result.is_ok(),
            "Merge should succeed after oplog sync. Error: {:?}",
            result.err()
        );

        // Verify content converged
        let final_content = client.get_block_snapshot(&block_id).unwrap().content;
        assert_eq!(final_content, "Initial content more text");
    }

    /// Test text streaming sync (append operations).
    #[test]
    fn test_text_streaming_sync() {
        // === Initial setup ===
        let mut server = BlockDocument::new("cell-1", "server-agent");
        let block_id = server.insert_block(
            None,
            None,
            Role::Model,
            BlockKind::Text,
            "", // Start empty
            "model:claude"
        ).unwrap();

        // Full sync to client
        let full_ops = server.ops_since(&[]);
        let mut client_oplog = OpLog::new();
        client_oplog.merge_ops_owned(full_ops).expect("initial sync");

        // === Streaming ===
        // Server appends text in chunks
        let chunks = ["Hello", " ", "World", "!"];

        for chunk in chunks {
            let frontier_before = server.frontier();
            server.append_text(&block_id, chunk).unwrap();

            let chunk_ops = server.ops_since(&frontier_before);
            client_oplog.merge_ops_owned(chunk_ops)
                .expect(&format!("merging chunk '{}' should work", chunk));
        }

        // Verify final state matches
        let server_content = server.get_block_snapshot(&block_id).unwrap().content;
        assert_eq!(server_content, "Hello World!");

        // For client verification, we'd need to wrap the oplog in BlockDocument
        // For now, just verify the merge succeeded without errors
    }
}
