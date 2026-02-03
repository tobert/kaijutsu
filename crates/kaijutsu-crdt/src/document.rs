//! Block document with unified CRDT using facet.
//!
//! Uses the facet library (github.com/tobert/facet) which provides a unified
//! Document API with Map, Set, Register, and Text CRDTs.
//!
//! # DAG Model
//!
//! Blocks form a DAG via parent_id links. Each block can have:
//! - A parent block (tool results under tool calls, responses under prompts)
//! - Multiple children (parallel tool calls, multiple response blocks)
//! - Role and status for conversation flow tracking

use facet::{Document, AgentId, SerializedOps, SerializedOpsOwned, LV};

use crate::{BlockId, BlockKind, BlockSnapshot, CrdtError, Result, Role, Status};

/// Block document backed by facet Document.
///
/// # Document Structure
///
/// ```text
/// ROOT (Map)
/// ├── blocks (Set<string>)          # OR-Set of block ID keys
/// ├── order:<id> -> i64             # Fractional index for ordering (scaled by 1M)
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
/// All operations go through the facet Document, ensuring:
/// - Maps use LWW (Last-Write-Wins) semantics
/// - Sets use OR-Set (add-wins) semantics
/// - Text uses sequence CRDT for character-level merging
/// - All peers converge to identical state after sync
pub struct BlockDocument {
    /// Document ID this document belongs to.
    document_id: String,

    /// Agent ID string for this instance.
    agent_id_str: String,

    /// Agent ID (numeric) in the Document.
    agent: AgentId,

    /// Facet Document containing all CRDT state.
    doc: Document,

    /// Next sequence number for block IDs (agent-local).
    next_seq: u64,

    /// Document version (incremented on each local operation).
    version: u64,
}

impl BlockDocument {
    /// Create a new empty document (server-side, creates initial structure).
    pub fn new(document_id: impl Into<String>, agent_id: impl Into<String>) -> Self {
        let document_id = document_id.into();
        let agent_id_str = agent_id.into();

        let mut doc = Document::new();
        let agent = doc.get_or_create_agent(&agent_id_str);

        // Create the blocks Set at ROOT["blocks"]
        doc.transact(agent, |tx| {
            tx.root().create_set("blocks");
        });

        Self {
            document_id,
            agent_id_str,
            agent,
            doc,
            next_seq: 0,
            version: 0,
        }
    }

    /// Create an empty document for sync (client-side, no initial operations).
    ///
    /// Use this when the document will receive its initial state via `merge_ops`.
    /// The blocks Set will be created when ops are merged from the server.
    pub fn new_for_sync(document_id: impl Into<String>, agent_id: impl Into<String>) -> Self {
        let document_id = document_id.into();
        let agent_id_str = agent_id.into();

        let mut doc = Document::new();
        let agent = doc.get_or_create_agent(&agent_id_str);

        Self {
            document_id,
            agent_id_str,
            agent,
            doc,
            next_seq: 0,
            version: 0,
        }
    }

    // =========================================================================
    // Accessors
    // =========================================================================

    /// Get the document ID.
    pub fn document_id(&self) -> &str {
        &self.document_id
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
        self.doc.get_set(&["blocks"])
            .map(|s| s.len())
            .unwrap_or(0)
    }

    /// Check if the document is empty.
    pub fn is_empty(&self) -> bool {
        self.block_count() == 0
    }

    /// Get block IDs in document order.
    fn block_ids_ordered(&self) -> Vec<BlockId> {
        // Get all block IDs from the Set
        let Some(blocks_set) = self.doc.get_set(&["blocks"]) else {
            return Vec::new();
        };

        // Collect (order_value, block_id) pairs
        let mut ordered: Vec<(f64, BlockId)> = blocks_set
            .iter()
            .filter_map(|v| {
                let key = v.as_str()?;
                let block_id = BlockId::from_key(key)?;
                let order_key = format!("order:{}", key);

                // Get order value from root map
                let order_val = self.doc.root().get(&order_key)
                    .and_then(|v| v.as_int())
                    .map(|n| n as f64 / 1_000_000.0)
                    .unwrap_or(0.0);

                Some((order_val, block_id))
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
        let block_map = self.doc.get_map(&[&block_key])?;

        // Extract fields from the block map
        let kind_str = block_map.get("kind")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "text".to_string());
        let kind = BlockKind::from_str(&kind_str).unwrap_or_default();

        let role_str = block_map.get("role")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "human".to_string());
        let role = Role::from_str(&role_str).unwrap_or_default();

        let status_str = block_map.get("status")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "done".to_string());
        let status = Status::from_str(&status_str).unwrap_or(Status::Done);

        let author = block_map.get("author")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_default();

        let created_at = block_map.get("created_at")
            .and_then(|v| v.as_int())
            .map(|n| n as u64)
            .unwrap_or(0);

        let content = block_map.get_text("content")
            .map(|t| t.content())
            .unwrap_or_default();

        let collapsed = block_map.get("collapsed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let parent_id = block_map.get("parent_id")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .and_then(|s| BlockId::from_key(&s));

        // Tool-specific fields
        let tool_name = block_map.get("tool_name")
            .and_then(|v| v.as_str().map(|s| s.to_string()));

        let tool_input = block_map.get_text("tool_input")
            .and_then(|t| {
                let json_str = t.content();
                serde_json::from_str(&json_str).ok()
            });

        let tool_call_id = block_map.get("tool_call_id")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .and_then(|s| BlockId::from_key(&s));

        let exit_code = block_map.get("exit_code")
            .and_then(|v| v.as_int())
            .map(|n| n as i32);

        let is_error = block_map.get("is_error")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let display_hint = block_map.get("display_hint")
            .and_then(|v| v.as_str().map(|s| s.to_string()));

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
            display_hint,
        })
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
        let id = BlockId::new(&self.document_id, &self.agent_id_str, self.next_seq);
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
                    let first_order = self.doc.root().get(&order_key)
                        .and_then(|v| v.as_int())
                        .map(|n| n as f64 / 1_000_000.0)
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
                        let after_order = self.doc.root().get(&order_key)
                            .and_then(|v| v.as_int())
                            .map(|n| n as f64 / 1_000_000.0)
                            .unwrap_or(1.0);

                        if idx + 1 < ordered.len() {
                            // There's a next block
                            let next_key = ordered[idx + 1].to_key();
                            let next_order_key = format!("order:{}", next_key);
                            let next_order = self.doc.root().get(&next_order_key)
                                .and_then(|v| v.as_int())
                                .map(|n| n as f64 / 1_000_000.0)
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
                            let last_order = self.doc.root().get(&order_key)
                                .and_then(|v| v.as_int())
                                .map(|n| n as f64 / 1_000_000.0)
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

    /// Check if a block exists in the Set.
    fn block_exists(&self, key: &str) -> bool {
        self.doc.get_set(&["blocks"])
            .map(|s| s.contains_str(key))
            .unwrap_or(false)
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
            None, // display_hint
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
            None, // display_hint
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

        // Default to placing after the tool call if not specified
        let after = after.or(Some(tool_call_id));

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
            None, // display_hint
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
            snapshot.display_hint,
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
        display_hint: Option<String>,
    ) -> Result<()> {
        let block_key = id.to_key();

        // Check for duplicate
        if self.block_exists(&block_key) {
            return Err(CrdtError::DuplicateBlock(id));
        }

        // Validate reference if provided
        if let Some(after_id) = after {
            if !self.block_exists(&after_id.to_key()) {
                return Err(CrdtError::InvalidReference(after_id.clone()));
            }
        }

        // Validate parent if provided
        if let Some(parent) = parent_id {
            if !self.block_exists(&parent.to_key()) {
                return Err(CrdtError::InvalidReference(parent.clone()));
            }
        }

        // Calculate order index
        let order_index = self.calc_order_index(after);

        // Get timestamp
        let created_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);

        // All mutations in a single transaction
        let block_map_key = format!("block:{}", block_key);
        let order_key = format!("order:{}", block_key);

        self.doc.transact(self.agent, |tx| {
            // Add block ID to the blocks Set
            if let Some(mut blocks_set) = tx.get_set_mut(&["blocks"]) {
                blocks_set.add_str(&block_key);
            }

            // Store order index (as i64 scaled by 1M for precision)
            tx.root().set(&order_key, (order_index * 1_000_000.0) as i64);

            // Create block map and collect text IDs to fill later
            // (We need to do this in two phases to satisfy the borrow checker)
            let (text_id, tool_input_id) = {
                let block_map_id = tx.root().create_map(&block_map_key);
                let mut block_map = tx.map_by_id(block_map_id);

                // Store block metadata
                block_map.set("author", author.as_str());
                block_map.set("created_at", created_at);
                block_map.set("kind", kind.as_str());
                block_map.set("role", role.as_str());
                block_map.set("status", Status::Done.as_str());
                block_map.set("collapsed", false);

                // Store parent_id if present
                if let Some(parent) = parent_id {
                    block_map.set("parent_id", parent.to_key().as_str());
                }

                // Create text CRDTs
                let text_id = block_map.create_text("content");

                // Tool-specific fields
                if let Some(ref name) = tool_name {
                    block_map.set("tool_name", name.as_str());
                }

                let tool_input_id = if tool_input.is_some() {
                    Some(block_map.create_text("tool_input"))
                } else {
                    None
                };

                if let Some(ref tcid) = tool_call_id {
                    block_map.set("tool_call_id", tcid.to_key().as_str());
                }

                if let Some(code) = exit_code {
                    block_map.set("exit_code", code as i64);
                }

                if is_error {
                    block_map.set("is_error", true);
                }

                if let Some(ref hint) = display_hint {
                    block_map.set("display_hint", hint.as_str());
                }

                (text_id, tool_input_id)
            };

            // Now fill in text content (block_map is dropped, so we can borrow tx again)
            if !content.is_empty() {
                if let Some(mut text) = tx.text_by_id(text_id) {
                    text.insert(0, &content);
                }
            }

            if let Some(ref input) = tool_input {
                if let Some(input_id) = tool_input_id {
                    let input_json = serde_json::to_string(input).unwrap_or_default();
                    if !input_json.is_empty() {
                        if let Some(mut input_text) = tx.text_by_id(input_id) {
                            input_text.insert(0, &input_json);
                        }
                    }
                }
            }
        });

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

        self.doc.transact(self.agent, |tx| {
            if let Some(mut block_map) = tx.get_map_mut(&[&block_key]) {
                block_map.set("status", status.as_str());
            }
        });

        self.version += 1;
        Ok(())
    }

    /// Set the display hint of a block.
    ///
    /// Display hints are used for richer output formatting (tables, trees).
    /// The hint is stored as a JSON string.
    pub fn set_display_hint(&mut self, id: &BlockId, hint: Option<&str>) -> Result<()> {
        let _snapshot = self.get_block_snapshot(id)
            .ok_or_else(|| CrdtError::BlockNotFound(id.clone()))?;

        let block_key = format!("block:{}", id.to_key());

        if let Some(h) = hint {
            self.doc.transact(self.agent, |tx| {
                if let Some(mut block_map) = tx.get_map_mut(&[&block_key]) {
                    block_map.set("display_hint", h);
                }
            });
        }
        // If hint is None, we don't need to do anything - it's already not set

        self.version += 1;
        Ok(())
    }

    /// Delete a block.
    pub fn delete_block(&mut self, id: &BlockId) -> Result<()> {
        let block_key = id.to_key();

        // Check if block exists
        if !self.block_exists(&block_key) {
            return Err(CrdtError::BlockNotFound(id.clone()));
        }

        // Remove from blocks Set
        self.doc.transact(self.agent, |tx| {
            if let Some(mut blocks_set) = tx.get_set_mut(&["blocks"]) {
                blocks_set.remove_str(&block_key);
            }
        });

        // Note: The block map and order entries remain in the oplog but are
        // effectively orphaned. This is fine for CRDT semantics.

        self.version += 1;
        Ok(())
    }

    // =========================================================================
    // Text Operations
    // =========================================================================

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
        let snapshot = self.get_block_snapshot(id)
            .ok_or_else(|| CrdtError::BlockNotFound(id.clone()))?;

        let block_key = format!("block:{}", id.to_key());

        // Validate position
        let len = snapshot.content.chars().count();
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
        self.doc.transact(self.agent, |tx| {
            if let Some(mut text) = tx.get_text_mut(&[&block_key, "content"]) {
                if delete > 0 {
                    text.delete(pos..pos + delete);
                }
                if !insert.is_empty() {
                    text.insert(pos, insert);
                }
            }
        });

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

        self.doc.transact(self.agent, |tx| {
            if let Some(mut block_map) = tx.get_map_mut(&[&block_key]) {
                block_map.set("collapsed", collapsed);
            }
        });

        self.version += 1;
        Ok(())
    }

    /// Move a block to a new position in the ordering.
    ///
    /// The block will be placed immediately after `after_id`, or at the
    /// beginning if `after_id` is None.
    pub fn move_block(&mut self, id: &BlockId, after: Option<&BlockId>) -> Result<()> {
        let block_key = id.to_key();

        // Check if block exists
        if !self.block_exists(&block_key) {
            return Err(CrdtError::BlockNotFound(id.clone()));
        }

        // Validate reference if provided
        if let Some(after_id) = after {
            if !self.block_exists(&after_id.to_key()) {
                return Err(CrdtError::InvalidReference(after_id.clone()));
            }
        }

        // Calculate new order index
        let order_index = self.calc_order_index(after);

        // Update order key
        let order_key = format!("order:{}", block_key);
        self.doc.transact(self.agent, |tx| {
            tx.root().set(&order_key, (order_index * 1_000_000.0) as i64);
        });

        self.version += 1;
        Ok(())
    }

    // =========================================================================
    // Sync Operations
    // =========================================================================

    /// Get operations since a frontier for replication.
    pub fn ops_since(&self, frontier: &[LV]) -> SerializedOpsOwned {
        self.doc.ops_since(frontier).into()
    }

    /// Merge remote operations.
    ///
    /// Use `ops_since()` to get operations, and pass them directly here.
    pub fn merge_ops(&mut self, ops: SerializedOps<'_>) -> Result<()> {
        self.doc.merge_ops_borrowed(ops)
            .map_err(|e| CrdtError::Internal(format!("merge error: {:?}", e)))?;

        self.refresh_after_merge();
        Ok(())
    }

    /// Merge remote operations (owned version for cross-thread/network use).
    ///
    /// Use this when receiving serialized ops that have been deserialized
    /// into the owned form (e.g., from network RPC).
    pub fn merge_ops_owned(&mut self, ops: SerializedOpsOwned) -> Result<()> {
        self.doc.merge_ops(ops)
            .map_err(|e| CrdtError::Internal(format!("merge error: {:?}", e)))?;

        self.refresh_after_merge();
        Ok(())
    }

    /// Refresh internal state after merging operations.
    fn refresh_after_merge(&mut self) {
        // Update next_seq if needed
        if let Some(blocks_set) = self.doc.get_set(&["blocks"]) {
            for v in blocks_set.iter() {
                if let Some(key) = v.as_str() {
                    if let Some(block_id) = BlockId::from_key(key) {
                        if block_id.agent_id == self.agent_id_str {
                            self.next_seq = self.next_seq.max(block_id.seq + 1);
                        }
                    }
                }
            }
        }

        self.version += 1;
    }

    /// Get the current frontier (version) for sync.
    pub fn frontier(&self) -> Vec<LV> {
        self.doc.version().as_ref().to_vec()
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
    /// we start with an empty Document and merge the server's full oplog.
    ///
    /// # Arguments
    ///
    /// * `document_id` - Document ID for the document
    /// * `agent_id` - Agent ID for local operations
    /// * `oplog_bytes` - Serialized oplog from server's `oplog_bytes()`
    ///
    /// # Returns
    ///
    /// A BlockDocument with the same oplog state as the server, ready for
    /// incremental sync via `merge_ops`.
    pub fn from_oplog(
        document_id: impl Into<String>,
        agent_id: impl Into<String>,
        oplog_bytes: &[u8],
    ) -> Result<Self> {
        let document_id = document_id.into();
        let agent_id_str = agent_id.into();

        // Start with empty document (no independent "blocks" Set!)
        let mut doc = Document::new();

        // Merge server's full oplog
        let ops: SerializedOpsOwned = serde_json::from_slice(oplog_bytes)
            .map_err(|e| CrdtError::Internal(format!("deserialize oplog: {}", e)))?;
        doc.merge_ops(ops)
            .map_err(|e| CrdtError::Internal(format!("merge oplog: {:?}", e)))?;

        // Verify blocks set exists
        if doc.get_set(&["blocks"]).is_none() {
            return Err(CrdtError::Internal(
                "oplog missing 'blocks' Set at root".into(),
            ));
        }

        let agent = doc.get_or_create_agent(&agent_id_str);

        // Calculate next_seq from existing blocks (avoid ID collisions)
        let mut next_seq = 0u64;
        if let Some(blocks_set) = doc.get_set(&["blocks"]) {
            for v in blocks_set.iter() {
                if let Some(key) = v.as_str() {
                    if let Some(block_id) = BlockId::from_key(key) {
                        if block_id.agent_id == agent_id_str {
                            next_seq = next_seq.max(block_id.seq + 1);
                        }
                    }
                }
            }
        }

        // Use block count as initial version - ensures non-zero if content exists
        // This makes sync detect the document has changed from an empty state
        let block_count = doc.get_set(&["blocks"]).map(|s| s.len()).unwrap_or(0);
        let version = if block_count > 0 { block_count as u64 } else { 0 };

        Ok(Self {
            document_id,
            agent_id_str,
            agent,
            doc,
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
            document_id: self.document_id.clone(),
            blocks: self.blocks_ordered(),
            version: self.version,
        }
    }

    /// Restore from a snapshot.
    pub fn from_snapshot(snapshot: DocumentSnapshot, agent_id: impl Into<String>) -> Self {
        let agent_id = agent_id.into();
        let mut doc = Self::new(&snapshot.document_id, &agent_id);

        let mut last_id: Option<BlockId> = None;
        for block_snap in &snapshot.blocks {
            // Track max seq for our agent_id to avoid ID collisions
            if block_snap.id.agent_id == agent_id {
                doc.next_seq = doc.next_seq.max(block_snap.id.seq + 1);
            }

            if doc.insert_block_with_id(
                block_snap.id.clone(),
                block_snap.parent_id.as_ref(),
                last_id.as_ref(),
                block_snap.role.clone(),
                block_snap.kind.clone(),
                block_snap.content.clone(),
                block_snap.author.clone(),
                block_snap.tool_name.clone(),
                block_snap.tool_input.clone(),
                block_snap.tool_call_id.clone(),
                block_snap.exit_code,
                block_snap.is_error,
                block_snap.display_hint.clone(),
            ).is_ok() {
                last_id = Some(block_snap.id.clone());
            }
        }

        doc.version = snapshot.version;
        doc
    }
}

/// Snapshot of a block document (serializable).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DocumentSnapshot {
    /// Document ID.
    pub document_id: String,
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
        let doc = BlockDocument::new("doc-1", "alice");
        assert_eq!(doc.document_id(), "doc-1");
        assert_eq!(doc.agent_id(), "alice");
        assert!(doc.is_empty());
        assert_eq!(doc.version(), 0);
    }

    #[test]
    fn test_insert_block_new_api() {
        let mut doc = BlockDocument::new("doc-1", "alice");

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
        let mut doc = BlockDocument::new("doc-1", "alice");

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
        let mut doc = BlockDocument::new("doc-1", "alice");

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
        let mut doc = BlockDocument::new("doc-1", "alice");

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
        let mut doc = BlockDocument::new("doc-1", "alice");

        let id1 = doc.insert_block(None, None, Role::User, BlockKind::Text, "First", "alice").unwrap();
        let id2 = doc.insert_block(None, Some(&id1), Role::User, BlockKind::Text, "Second", "alice").unwrap();
        let id3 = doc.insert_block(None, Some(&id2), Role::User, BlockKind::Text, "Third", "alice").unwrap();

        let order: Vec<_> = doc.blocks_ordered().iter().map(|b| b.id.clone()).collect();
        assert_eq!(order, vec![id1, id2, id3]);
    }

    #[test]
    fn test_insert_at_beginning() {
        let mut doc = BlockDocument::new("doc-1", "alice");

        let id1 = doc.insert_block(None, None, Role::User, BlockKind::Text, "First", "alice").unwrap();
        let id2 = doc.insert_block(None, None, Role::User, BlockKind::Text, "Before First", "alice").unwrap();

        let order: Vec<_> = doc.blocks_ordered().iter().map(|b| b.id.clone()).collect();
        assert_eq!(order, vec![id2, id1]);
    }

    #[test]
    fn test_snapshot_roundtrip() {
        let mut doc = BlockDocument::new("doc-1", "alice");

        doc.insert_block(None, None, Role::Model, BlockKind::Thinking, "Thinking...", "model:claude").unwrap();
        doc.insert_block(None, None, Role::Model, BlockKind::Text, "Response", "model:claude").unwrap();

        let snapshot = doc.snapshot();
        let restored = BlockDocument::from_snapshot(snapshot.clone(), "bob");

        assert_eq!(restored.block_count(), doc.block_count());
        assert_eq!(restored.full_text(), doc.full_text());
    }

    #[test]
    fn test_text_editing() {
        let mut doc = BlockDocument::new("doc-1", "alice");

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
        let mut doc = BlockDocument::new("doc-1", "alice");

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
        let fake_id = BlockId::new("doc-1", "fake", 999);
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
        let mut server = BlockDocument::new("doc-1", "server-agent");

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
        let mut client = BlockDocument::new("doc-1", "client-agent");

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
        use facet::Document;

        // === Server side ===
        let mut server = BlockDocument::new("doc-1", "server-agent");

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
        // Create a new Document directly to simulate a clean sync client
        let mut client_doc = Document::new();

        // Merge the full ops - this should work because it includes "blocks" Set creation
        let result = client_doc.merge_ops(full_ops);

        assert!(
            result.is_ok(),
            "Full oplog merge should succeed: {:?}", result
        );

        // Verify client doc has the blocks key
        assert!(
            client_doc.root().contains_key("blocks"),
            "Client should have 'blocks' key after merging full oplog"
        );
    }

    /// Test incremental sync after initial full sync.
    ///
    /// After client receives full oplog and establishes sync, subsequent
    /// incremental ops should merge correctly.
    #[test]
    fn test_incremental_sync_after_full_sync() {
        use facet::Document;

        // === Initial full sync ===
        let mut server = BlockDocument::new("doc-1", "server-agent");
        let full_ops = server.ops_since(&[]);

        // Client creates empty doc and merges full state
        let mut client_doc = Document::new();
        client_doc.merge_ops(full_ops).expect("initial sync should work");

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
        let result = client_doc.merge_ops(incremental_ops);

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
        let mut server = BlockDocument::new("doc-1", "server-agent");
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
        let mut client = BlockDocument::from_oplog("doc-1", "client-agent", &oplog_bytes)
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
        use facet::Document;

        // === Initial setup ===
        let mut server = BlockDocument::new("doc-1", "server-agent");
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
        let mut client_doc = Document::new();
        client_doc.merge_ops(full_ops).expect("initial sync");

        // === Streaming ===
        // Server appends text in chunks
        let chunks = ["Hello", " ", "World", "!"];

        for chunk in chunks {
            let frontier_before = server.frontier();
            server.append_text(&block_id, chunk).unwrap();

            let chunk_ops = server.ops_since(&frontier_before);
            client_doc.merge_ops(chunk_ops)
                .expect(&format!("merging chunk '{}' should work", chunk));
        }

        // Verify final state matches
        let server_content = server.get_block_snapshot(&block_id).unwrap().content;
        assert_eq!(server_content, "Hello World!");

        // For client verification, we'd need to wrap the doc in BlockDocument
        // For now, just verify the merge succeeded without errors
    }
}
