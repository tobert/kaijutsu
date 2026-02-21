//! Block document with unified CRDT using diamond-types-extended.
//!
//! Uses the diamond-types-extended library (github.com/tobert/diamond-types-extended)
//! which provides a unified Document API with Map, Set, Register, and Text CRDTs.
//!
//! # DAG Model
//!
//! Blocks form a DAG via parent_id links. Each block can have:
//! - A parent block (tool results under tool calls, responses under prompts)
//! - Multiple children (parallel tool calls, multiple response blocks)
//! - Role and status for conversation flow tracking

use diamond_types_extended::{Document, AgentId, Frontier, SerializedOps, SerializedOpsOwned, Uuid};

use crate::{BlockId, BlockKind, BlockSnapshot, ContextId, CrdtError, PrincipalId, Result, Role, Status, ToolKind};

/// Base-62 charset for fractional indexing (0-9, A-Z, a-z).
/// Lexicographically ordered: '0' < '9' < 'A' < 'Z' < 'a' < 'z'.
const BASE62: &[u8; 62] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz";

/// Get the index of a character in the BASE62 charset.
fn base62_index(c: u8) -> usize {
    BASE62.iter().position(|&b| b == c).unwrap_or(0)
}

/// Compute a lexicographic midpoint between two base-62 strings.
///
/// Empty string `""` sorts before everything. Both `a` and `b` must satisfy `a < b`
/// lexicographically. The result is guaranteed to satisfy `a < result < b`.
fn order_midpoint(a: &str, b: &str) -> String {
    let a_bytes = a.as_bytes();
    let b_bytes = b.as_bytes();
    let max_len = a_bytes.len().max(b_bytes.len());

    let mut result = Vec::new();

    for i in 0..=max_len {
        let a_val = if i < a_bytes.len() { base62_index(a_bytes[i]) } else { 0 };
        let b_val = if i < b_bytes.len() { base62_index(b_bytes[i]) } else { 62 };

        if a_val + 1 < b_val {
            // There's room between a and b at this position
            let mid = (a_val + b_val) / 2;
            result.push(BASE62[mid]);
            return String::from_utf8(result).unwrap_or_else(|_| "V".to_string());
        } else if a_val == b_val {
            // Same character — carry it and continue to next position
            result.push(BASE62[a_val]);
        } else {
            // a_val + 1 == b_val (adjacent): carry a_val and find midpoint in next position
            result.push(BASE62[a_val]);
            // Now we need midpoint between a[i+1..] and "z" (end of range)
            let a_next = if i + 1 < a_bytes.len() { base62_index(a_bytes[i + 1]) } else { 0 };
            let mid = (a_next + 62) / 2;
            result.push(BASE62[mid]);
            return String::from_utf8(result).unwrap_or_else(|_| "V".to_string());
        }
    }

    // Fallback: append midpoint character
    result.push(BASE62[31]); // 'V'
    String::from_utf8(result).unwrap_or_else(|_| "V".to_string())
}

/// Block document backed by diamond-types-extended Document.
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
///     ├── created_at (I64)
///     │
///     │   # Tool-specific (optional, set on creation)
///     ├── tool_kind (Str)           # For ToolCall/ToolResult (shell/mcp/builtin)
///     ├── tool_name (Str)           # For ToolCall
///     ├── tool_input (Text)         # For ToolCall - JSON, streamable
///     ├── tool_call_id (Str)        # For ToolResult → parent ToolCall
///     ├── exit_code (I64)           # For ToolResult
///     └── is_error (Bool)           # For ToolResult
/// ```
///
/// # Convergence
///
/// All operations go through the diamond-types-extended Document, ensuring:
/// - Maps use LWW (Last-Write-Wins) semantics
/// - Sets use OR-Set (add-wins) semantics
/// - Text uses sequence CRDT for character-level merging
/// - All peers converge to identical state after sync
pub struct BlockDocument {
    /// Context ID this document belongs to.
    context_id: ContextId,

    /// Principal ID for this agent instance.
    agent_id: PrincipalId,

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
    pub fn new(context_id: ContextId, agent_id: PrincipalId) -> Self {
        let mut doc = Document::new();
        let dte_uuid = Uuid::from_bytes(*agent_id.as_bytes());
        let agent = doc.create_agent(dte_uuid);

        // Create the blocks Set at ROOT["blocks"]
        doc.transact(agent, |tx| {
            tx.root().create_set("blocks");
        });

        Self {
            context_id,
            agent_id,
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
    pub fn new_for_sync(context_id: ContextId, agent_id: PrincipalId) -> Self {
        let mut doc = Document::new();
        let dte_uuid = Uuid::from_bytes(*agent_id.as_bytes());
        let agent = doc.create_agent(dte_uuid);

        Self {
            context_id,
            agent_id,
            agent,
            doc,
            next_seq: 0,
            version: 0,
        }
    }

    // =========================================================================
    // Accessors
    // =========================================================================

    /// Get the context ID.
    pub fn context_id(&self) -> ContextId {
        self.context_id
    }

    /// Get the agent/principal ID.
    pub fn agent_id(&self) -> PrincipalId {
        self.agent_id
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
    pub fn block_ids_ordered(&self) -> Vec<BlockId> {
        // Get all block IDs from the Set
        let Some(blocks_set) = self.doc.get_set(&["blocks"]) else {
            return Vec::new();
        };

        // Collect keys first, then drop the Set iterator before querying root.
        // The Set iterator borrows internal document state; querying root().get()
        // needs the same state — holding both causes a re-entrant lock deadlock
        // with DTE v0.2's interior mutability.
        let keys: Vec<String> = blocks_set
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
        #[allow(clippy::drop_non_drop)] // intentional: release DTE interior lock
        drop(blocks_set);

        // Now safe to query root map for order values
        let mut ordered: Vec<(String, BlockId)> = keys
            .into_iter()
            .filter_map(|key| {
                let block_id = BlockId::from_key(&key)?;
                let order_key = format!("order:{}", key);

                let order_val = self.doc.root().get(&order_key)
                    .and_then(|v| {
                        // Try string first (new format), fall back to i64 (legacy)
                        if let Some(s) = v.as_str() {
                            return Some(s.to_string());
                        }
                        if let Some(n) = v.as_int() {
                            return Some(format!("{:020}", n));
                        }
                        None
                    })
                    .unwrap_or_default();

                Some((order_val, block_id))
            })
            .collect();

        // Sort by string order key (lexicographic)
        ordered.sort_by(|a, b| a.0.cmp(&b.0));

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
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let kind = match &kind_str {
            Some(s) => match BlockKind::from_str(s) {
                Some(k) => k,
                None => {
                    tracing::warn!("block {} has unparseable kind {:?}, skipping", id.to_key(), s);
                    return None;
                }
            },
            None => {
                tracing::warn!("block {} missing 'kind' field, skipping", id.to_key());
                return None;
            }
        };

        let role_str = block_map.get("role")
            .and_then(|v| v.as_str().map(|s| s.to_string()));
        let role = match &role_str {
            Some(s) => match Role::from_str(s) {
                Some(r) => r,
                None => {
                    tracing::warn!("block {} has unparseable role {:?}, skipping", id.to_key(), s);
                    return None;
                }
            },
            None => {
                tracing::warn!("block {} missing 'role' field, skipping", id.to_key());
                return None;
            }
        };

        let status_str = block_map.get("status")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| "done".to_string());
        let status = Status::from_str(&status_str).unwrap_or(Status::Done);

        let created_at = block_map.get("created_at")
            .and_then(|v| v.as_int())
            .map(|n| n as u64)
            .unwrap_or(0);

        // Prefer content_final (LWW register, 1 LV) over content (Text CRDT, 1 LV/char)
        let content = block_map.get("content_final")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| {
                block_map.get_text("content")
                    .map(|t| t.content())
                    .unwrap_or_default()
            });

        let collapsed = block_map.get("collapsed")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let compacted = block_map.get("compacted")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let parent_id = block_map.get("parent_id")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .and_then(|s| BlockId::from_key(&s));

        // Tool-specific fields
        let tool_kind = block_map.get("tool_kind")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .and_then(|s| ToolKind::from_str(&s));

        let tool_name = block_map.get("tool_name")
            .and_then(|v| v.as_str().map(|s| s.to_string()));

        let tool_input = block_map.get_text("tool_input")
            .map(|t| t.content())
            .filter(|s| !s.is_empty());

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

        // Drift-specific fields
        let source_context = block_map.get("source_context")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .and_then(|s| ContextId::parse(&s).ok());

        let source_model = block_map.get("source_model")
            .and_then(|v| v.as_str().map(|s| s.to_string()));

        let drift_kind = block_map.get("drift_kind")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .and_then(|s| crate::DriftKind::from_str(&s));

        Some(BlockSnapshot {
            id: *id,
            parent_id,
            role,
            status,
            kind,
            content,
            collapsed,
            compacted,
            created_at,
            tool_kind,
            tool_name,
            tool_input,
            tool_call_id,
            exit_code,
            is_error,
            display_hint,
            source_context,
            source_model,
            drift_kind,
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
                ancestors.push(parent_id);
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
        let id = BlockId::new(self.context_id, self.agent_id, self.next_seq);
        self.next_seq += 1;
        id
    }

    /// Read the stored order key for a block as a string.
    ///
    /// Supports both new string format (as_str) and legacy i64 format (as_int,
    /// converted to zero-padded string for backward compatibility).
    fn get_block_order_key(&self, block_id: &BlockId, default: &str) -> String {
        let order_key = format!("order:{}", block_id.to_key());
        self.doc.root().get(&order_key)
            .and_then(|v| {
                // Try string first (new format)
                if let Some(s) = v.as_str() {
                    return Some(s.to_string());
                }
                // Fall back to i64 (legacy format) — convert to zero-padded string
                if let Some(n) = v.as_int() {
                    return Some(format!("{:020}", n));
                }
                None
            })
            .unwrap_or_else(|| default.to_string())
    }

    /// Calculate a string-based fractional index for insertion.
    ///
    /// Uses base-62 lexicographic midpoint for unlimited precision.
    fn calc_order_key(&self, after: Option<&BlockId>) -> String {
        let ordered = self.block_ids_ordered();

        match after {
            None => {
                // Insert at beginning
                if ordered.is_empty() {
                    "V".to_string() // midpoint of base-62 range
                } else {
                    let first_order = self.get_block_order_key(&ordered[0], "V");
                    order_midpoint("", &first_order)
                }
            }
            Some(after_id) => {
                let after_idx = ordered.iter().position(|id| id == after_id);
                match after_idx {
                    Some(idx) => {
                        let after_order = self.get_block_order_key(&ordered[idx], "V");
                        if idx + 1 < ordered.len() {
                            let next_order = self.get_block_order_key(&ordered[idx + 1], &format!("{}~", after_order));
                            order_midpoint(&after_order, &next_order)
                        } else {
                            // Append after last — just add a suffix
                            format!("{}V", after_order)
                        }
                    }
                    None => {
                        if let Some(last) = ordered.last() {
                            let last_order = self.get_block_order_key(last, "V");
                            format!("{}V", last_order)
                        } else {
                            "V".to_string()
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
    /// Author is implicit — derived from `self.agent_id` via the BlockId.
    ///
    /// # Arguments
    ///
    /// * `parent_id` - Parent block ID for DAG relationship (None for root)
    /// * `after` - Block ID to insert after in document order (None for beginning)
    /// * `role` - Role of the block author (Human, Agent, System, Tool)
    /// * `kind` - Content type (Text, Thinking, ToolCall, ToolResult)
    /// * `content` - Initial text content
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
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let content_str = content.into();

        self.insert_block_with_id(
            id,
            parent_id,
            after,
            role,
            kind,
            content_str,
            None, // tool_kind
            None, // tool_name
            None, // tool_input
            None, // tool_call_id
            None, // exit_code
            false, // is_error
            false, // compacted
            None, // display_hint
            None, // source_context
            None, // source_model
            None, // drift_kind
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
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let input_json = serde_json::to_string_pretty(&tool_input)
            .map_err(|e| CrdtError::Serialization(e.to_string()))?;

        self.insert_block_with_id(
            id,
            parent_id,
            after,
            Role::Model,
            BlockKind::ToolCall,
            input_json.clone(),
            None, // tool_kind
            Some(tool_name.into()),
            Some(input_json),
            None,
            None,
            false,
            false, // compacted
            None, // display_hint
            None, // source_context
            None, // source_model
            None, // drift_kind
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
    ) -> Result<BlockId> {
        let id = self.new_block_id();

        // Default to placing after the tool call if not specified
        let after = after.or(Some(tool_call_id));

        self.insert_block_with_id(
            id,
            Some(tool_call_id), // Parent is the tool call
            after,
            Role::Tool,
            BlockKind::ToolResult,
            content.into(),
            None, // tool_kind
            None,
            None,
            Some(*tool_call_id),
            exit_code,
            is_error,
            false, // compacted
            None, // display_hint
            None, // source_context
            None, // source_model
            None, // drift_kind
        )?;

        Ok(id)
    }

    /// Insert a block from a complete snapshot (for remote sync).
    ///
    /// This is used when receiving blocks from the server via block events.
    /// The snapshot contains all fields including the pre-assigned block ID.
    pub fn insert_from_snapshot(
        &mut self,
        snapshot: BlockSnapshot,
        after: Option<&BlockId>,
    ) -> Result<BlockId> {
        // Generate a local ID if the snapshot has a placeholder (nil context_id).
        // This happens for drift blocks built by DriftRouter::build_drift_block().
        let block_id = if snapshot.id.context_id.is_nil() {
            self.new_block_id()
        } else {
            // Update next_seq if needed to avoid collisions with remote IDs
            if snapshot.id.agent_id == self.agent_id {
                self.next_seq = self.next_seq.max(snapshot.id.seq + 1);
            }
            snapshot.id
        };

        self.insert_block_with_id(
            block_id,
            snapshot.parent_id.as_ref(),
            after,
            snapshot.role,
            snapshot.kind,
            snapshot.content,
            snapshot.tool_kind,
            snapshot.tool_name,
            snapshot.tool_input,
            snapshot.tool_call_id,
            snapshot.exit_code,
            snapshot.is_error,
            snapshot.compacted,
            snapshot.display_hint,
            snapshot.source_context,
            snapshot.source_model,
            snapshot.drift_kind,
        )?;

        Ok(block_id)
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
        tool_kind: Option<ToolKind>,
        tool_name: Option<String>,
        tool_input: Option<String>,
        tool_call_id: Option<BlockId>,
        exit_code: Option<i32>,
        is_error: bool,
        compacted: bool,
        display_hint: Option<String>,
        source_context: Option<ContextId>,
        source_model: Option<String>,
        drift_kind: Option<crate::DriftKind>,
    ) -> Result<()> {
        let block_key = id.to_key();

        // Check for duplicate
        if self.block_exists(&block_key) {
            return Err(CrdtError::DuplicateBlock(id));
        }

        // Validate reference if provided
        if let Some(after_id) = after
            && !self.block_exists(&after_id.to_key())
        {
            return Err(CrdtError::InvalidReference(*after_id));
        }

        // Validate parent if provided
        if let Some(parent) = parent_id
            && !self.block_exists(&parent.to_key())
        {
            return Err(CrdtError::InvalidReference(*parent));
        }

        // Calculate order key (string-based fractional index)
        let order_val = self.calc_order_key(after);

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

            // Store order as string for unlimited precision
            tx.root().set(&order_key, order_val.as_str());

            // Create block map and collect text IDs to fill later
            // (We need to do this in two phases to satisfy the borrow checker)
            let (text_id, tool_input_id) = {
                let block_map_id = tx.root().create_map(&block_map_key);
                let mut block_map = tx.map_by_id(block_map_id);

                // Store block metadata (no author — derived from id.agent_id)
                block_map.set("created_at", created_at);
                block_map.set("kind", kind.as_str());
                block_map.set("role", role.as_str());
                block_map.set("status", Status::Done.as_str());
                block_map.set("collapsed", false);

                // Store compacted flag (only when true)
                if compacted {
                    block_map.set("compacted", true);
                }

                // Store parent_id if present
                if let Some(parent) = parent_id {
                    block_map.set("parent_id", parent.to_key().as_str());
                }

                // Create text CRDTs
                let text_id = block_map.create_text("content");

                // Tool-specific fields
                if let Some(ref tk) = tool_kind {
                    block_map.set("tool_kind", tk.as_str());
                }

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

                // Drift-specific fields
                if let Some(ctx) = source_context {
                    block_map.set("source_context", ctx.to_hex().as_str());
                }
                if let Some(ref model) = source_model {
                    block_map.set("source_model", model.as_str());
                }
                if let Some(ref dk) = drift_kind {
                    block_map.set("drift_kind", dk.as_str());
                }

                (text_id, tool_input_id)
            };

            // Now fill in text content (block_map is dropped, so we can borrow tx again)
            if !content.is_empty()
                && let Some(mut text) = tx.text_by_id(text_id)
            {
                text.insert(0, &content);
            }

            if let Some(ref input) = tool_input
                && let Some(input_id) = tool_input_id
                && !input.is_empty()
                && let Some(mut input_text) = tx.text_by_id(input_id)
            {
                input_text.insert(0, input);
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
            .ok_or(CrdtError::BlockNotFound(*id))?;

        let block_key = format!("block:{}", id.to_key());

        self.doc.transact(self.agent, |tx| {
            if let Some(mut block_map) = tx.get_map_mut(&[&block_key]) {
                block_map.set("status", status.as_str());
            }
        });

        self.version += 1;
        Ok(())
    }

    /// Promote a block's content from Text CRDT to an LWW register.
    ///
    /// Reads the current Text CRDT content and writes it as a `content_final`
    /// map key (LWW string via `map.set()`). This consumes 1 LV total instead
    /// of 1 LV per character, significantly reducing oplog size for finalized blocks.
    ///
    /// Should be called when a block transitions to Done or Error status.
    /// Idempotent — re-promoting overwrites with the same content.
    pub fn promote_to_register(&mut self, id: &BlockId) -> Result<()> {
        let block_key = format!("block:{}", id.to_key());

        // Read current text content
        let content = {
            let block_map = self.doc.get_map(&[&block_key])
                .ok_or(CrdtError::BlockNotFound(*id))?;
            block_map.get_text("content")
                .map(|t| t.content())
                .unwrap_or_default()
        };

        // Write as LWW register
        self.doc.transact(self.agent, |tx| {
            if let Some(mut block_map) = tx.get_map_mut(&[&block_key]) {
                block_map.set("content_final", content.as_str());
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
            .ok_or(CrdtError::BlockNotFound(*id))?;

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
            return Err(CrdtError::BlockNotFound(*id));
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
            .ok_or(CrdtError::BlockNotFound(*id))?;

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
            .ok_or(CrdtError::BlockNotFound(*id))?;

        // Use chars().count() for UTF-8 safety - edit_text uses character positions
        let len = snapshot.content.chars().count();
        self.edit_text(id, len, text, 0)
    }

    /// Set collapsed state of a thinking block.
    ///
    /// Only Thinking blocks support the collapsed state.
    pub fn set_collapsed(&mut self, id: &BlockId, collapsed: bool) -> Result<()> {
        let snapshot = self.get_block_snapshot(id)
            .ok_or(CrdtError::BlockNotFound(*id))?;

        if snapshot.kind != BlockKind::Thinking {
            return Err(CrdtError::UnsupportedOperation(*id));
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
            return Err(CrdtError::BlockNotFound(*id));
        }

        // Validate reference if provided
        if let Some(after_id) = after
            && !self.block_exists(&after_id.to_key())
        {
            return Err(CrdtError::InvalidReference(*after_id));
        }

        // Calculate new order key
        let order_val = self.calc_order_key(after);

        // Update order key
        let order_key = format!("order:{}", block_key);
        self.doc.transact(self.agent, |tx| {
            tx.root().set(&order_key, order_val.as_str());
        });

        self.version += 1;
        Ok(())
    }

    // =========================================================================
    // Sync Operations
    // =========================================================================

    /// Get operations since a frontier for replication.
    pub fn ops_since(&self, frontier: &Frontier) -> SerializedOpsOwned {
        self.doc.ops_since_owned(frontier)
    }

    /// Merge remote operations.
    ///
    /// Use `ops_since()` to get operations, and pass them directly here.
    /// Wraps the merge in catch_unwind to handle DTE causalgraph panics gracefully.
    pub fn merge_ops(&mut self, ops: SerializedOps<'_>) -> Result<()> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.doc.merge_ops_borrowed(ops)
        }));
        match result {
            Ok(Ok(())) => {
                self.refresh_after_merge();
                Ok(())
            }
            Ok(Err(e)) => Err(CrdtError::Internal(format!("merge error: {:?}", e))),
            Err(_) => Err(CrdtError::Internal(
                "CRDT merge panicked — likely concurrent causalgraph bug in DTE".into(),
            )),
        }
    }

    /// Merge remote operations (owned version for cross-thread/network use).
    ///
    /// Use this when receiving serialized ops that have been deserialized
    /// into the owned form (e.g., from network RPC).
    /// Wraps the merge in catch_unwind to handle DTE causalgraph panics gracefully.
    pub fn merge_ops_owned(&mut self, ops: SerializedOpsOwned) -> Result<()> {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.doc.merge_ops(ops)
        }));
        match result {
            Ok(Ok(())) => {
                self.refresh_after_merge();
                Ok(())
            }
            Ok(Err(e)) => Err(CrdtError::Internal(format!("merge error: {:?}", e))),
            Err(_) => Err(CrdtError::Internal(
                "CRDT merge panicked — likely concurrent causalgraph bug in DTE".into(),
            )),
        }
    }

    /// Refresh internal state after merging operations.
    fn refresh_after_merge(&mut self) {
        // Collect keys first, then drop the Set iterator before any further queries.
        // Same defensive pattern as block_ids_ordered() — avoids re-entrant lock.
        if let Some(blocks_set) = self.doc.get_set(&["blocks"]) {
            let keys: Vec<String> = blocks_set
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            #[allow(clippy::drop_non_drop)] // intentional: release DTE interior lock
            drop(blocks_set);
            for key in keys {
                if let Some(block_id) = BlockId::from_key(&key)
                    && block_id.agent_id == self.agent_id
                {
                    self.next_seq = self.next_seq.max(block_id.seq + 1);
                }
            }
        }

        self.version += 1;
    }

    /// Get the current frontier (version) for sync.
    pub fn frontier(&self) -> Frontier {
        self.doc.version().clone()
    }

    // =========================================================================
    // Fork Support
    // =========================================================================

    /// Fork the document, creating a copy with a new context ID.
    ///
    /// All blocks and their content are copied to the new document.
    /// The new document gets a fresh agent ID for future edits.
    pub fn fork(&self, new_context_id: ContextId, new_agent_id: PrincipalId) -> Self {
        let mut forked = BlockDocument::new(new_context_id, new_agent_id);

        // Copy all blocks in order
        let blocks = self.blocks_ordered();
        let mut id_mapping: std::collections::HashMap<BlockId, BlockId> = std::collections::HashMap::new();
        let mut last_id: Option<BlockId> = None;

        for block in blocks {
            let new_id = forked.new_block_id();
            id_mapping.insert(block.id, new_id);

            let new_parent_id = block.parent_id
                .and_then(|old_pid| id_mapping.get(&old_pid).copied());
            let new_tool_call_id = block.tool_call_id
                .and_then(|old_tcid| id_mapping.get(&old_tcid).copied());

            let _ = forked.insert_block_with_id(
                new_id,
                new_parent_id.as_ref(),
                last_id.as_ref(),
                block.role,
                block.kind,
                block.content,
                block.tool_kind,
                block.tool_name,
                block.tool_input,
                new_tool_call_id,
                block.exit_code,
                block.is_error,
                block.compacted,
                block.display_hint,
                block.source_context,
                block.source_model,
                block.drift_kind,
            );

            last_id = Some(new_id);
        }

        forked
    }

    /// Fork the document at a specific version, excluding blocks created after that version.
    pub fn fork_at_version(&self, new_context_id: ContextId, new_agent_id: PrincipalId, at_version: u64) -> Self {
        let mut forked = BlockDocument::new(new_context_id, new_agent_id);

        let blocks: Vec<_> = self.blocks_ordered()
            .into_iter()
            .filter(|b| b.created_at <= at_version)
            .collect();

        let mut id_mapping: std::collections::HashMap<BlockId, BlockId> = std::collections::HashMap::new();
        let mut last_id: Option<BlockId> = None;

        for block in blocks {
            let new_id = forked.new_block_id();
            id_mapping.insert(block.id, new_id);

            let new_parent_id = block.parent_id
                .and_then(|old_pid| id_mapping.get(&old_pid).copied());
            let new_tool_call_id = block.tool_call_id
                .and_then(|old_tcid| id_mapping.get(&old_tcid).copied());

            let _ = forked.insert_block_with_id(
                new_id,
                new_parent_id.as_ref(),
                last_id.as_ref(),
                block.role,
                block.kind,
                block.content,
                block.tool_kind,
                block.tool_name,
                block.tool_input,
                new_tool_call_id,
                block.exit_code,
                block.is_error,
                block.compacted,
                block.display_hint,
                block.source_context,
                block.source_model,
                block.drift_kind,
            );

            last_id = Some(new_id);
        }

        forked
    }

    // =========================================================================
    // Oplog-Based Sync (for initial state transfer)
    // =========================================================================

    /// Get full oplog as serialized bytes (for initial sync).
    pub fn oplog_bytes(&self) -> Result<Vec<u8>> {
        let ops = self.ops_since(&Frontier::root());
        postcard::to_stdvec(&ops)
            .map_err(|e| CrdtError::Serialization(e.to_string()))
    }

    /// Create document from serialized oplog (client-side sync).
    pub fn from_oplog(
        context_id: ContextId,
        agent_id: PrincipalId,
        oplog_bytes: &[u8],
    ) -> Result<Self> {
        let mut doc = Document::new();

        let ops: SerializedOpsOwned = postcard::from_bytes(oplog_bytes)
            .map_err(|e| CrdtError::Internal(format!("deserialize oplog: {}", e)))?;
        doc.merge_ops(ops)
            .map_err(|e| CrdtError::Internal(format!("merge oplog: {:?}", e)))?;

        if doc.get_set(&["blocks"]).is_none() {
            return Err(CrdtError::Internal(
                "oplog missing 'blocks' Set at root".into(),
            ));
        }

        let dte_uuid = Uuid::from_bytes(*agent_id.as_bytes());
        let agent = doc.create_agent(dte_uuid);

        // Calculate next_seq from existing blocks (avoid ID collisions).
        let mut next_seq = 0u64;
        if let Some(blocks_set) = doc.get_set(&["blocks"]) {
            let keys: Vec<String> = blocks_set
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            #[allow(clippy::drop_non_drop)] // intentional: release DTE interior lock
            drop(blocks_set);
            for key in keys {
                if let Some(block_id) = BlockId::from_key(&key)
                    && block_id.agent_id == agent_id
                {
                    next_seq = next_seq.max(block_id.seq + 1);
                }
            }
        }

        let block_count = doc.get_set(&["blocks"]).map(|s| s.len()).unwrap_or(0);
        let version = if block_count > 0 { block_count as u64 } else { 0 };

        Ok(Self {
            context_id,
            agent_id,
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
            context_id: self.context_id,
            blocks: self.blocks_ordered(),
            version: self.version,
        }
    }

    /// Compact the oplog by rebuilding from a snapshot of current state.
    pub fn compact(&mut self) -> Result<Frontier> {
        let snapshot = self.snapshot();
        let compacted = Self::from_snapshot(snapshot, self.agent_id);
        self.doc = compacted.doc;
        self.agent = compacted.agent;
        self.next_seq = compacted.next_seq;
        Ok(self.frontier())
    }

    /// Restore from a snapshot.
    pub fn from_snapshot(snapshot: DocumentSnapshot, agent_id: PrincipalId) -> Self {
        let mut doc = Self::new(snapshot.context_id, agent_id);

        let mut last_id: Option<BlockId> = None;
        let mut finalized_content: Vec<(BlockId, String)> = Vec::new();

        for block_snap in &snapshot.blocks {
            // Track max seq for our agent_id to avoid ID collisions
            if block_snap.id.agent_id == agent_id {
                doc.next_seq = doc.next_seq.max(block_snap.id.seq + 1);
            }

            // For Done/Error blocks, skip Text CRDT content — use register instead
            let is_finalized = matches!(block_snap.status, Status::Done | Status::Error);
            let content = if is_finalized && !block_snap.content.is_empty() {
                finalized_content.push((block_snap.id, block_snap.content.clone()));
                String::new() // skip Text CRDT fill
            } else {
                block_snap.content.clone()
            };

            if doc.insert_block_with_id(
                block_snap.id,
                block_snap.parent_id.as_ref(),
                last_id.as_ref(),
                block_snap.role,
                block_snap.kind,
                content,
                block_snap.tool_kind,
                block_snap.tool_name.clone(),
                block_snap.tool_input.clone(),
                block_snap.tool_call_id,
                block_snap.exit_code,
                block_snap.is_error,
                block_snap.compacted,
                block_snap.display_hint.clone(),
                block_snap.source_context,
                block_snap.source_model.clone(),
                block_snap.drift_kind,
            ).is_ok() {
                last_id = Some(block_snap.id);
            }
        }

        // Write content_final registers for finalized blocks (1 LV each vs 1 LV/char)
        for (id, content) in finalized_content {
            let block_key = format!("block:{}", id.to_key());
            doc.doc.transact(doc.agent, |tx| {
                if let Some(mut block_map) = tx.get_map_mut(&[&block_key]) {
                    block_map.set("content_final", content.as_str());
                }
            });
        }

        doc.version = snapshot.version;
        doc
    }
}

/// Snapshot of a block document (serializable).
#[derive(Clone, Debug, serde::Serialize, serde::Deserialize)]
pub struct DocumentSnapshot {
    /// Context ID.
    pub context_id: ContextId,
    /// Blocks in order.
    pub blocks: Vec<BlockSnapshot>,
    /// Version.
    pub version: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: create a test document with typed IDs.
    fn test_doc() -> BlockDocument {
        BlockDocument::new(ContextId::new(), PrincipalId::new())
    }

    /// Helper: create a test document pair with different agents.
    fn test_doc_pair() -> (BlockDocument, BlockDocument) {
        let ctx = ContextId::new();
        let alice = PrincipalId::new();
        let bob = PrincipalId::new();
        (BlockDocument::new(ctx, alice), BlockDocument::new(ctx, bob))
    }

    #[test]
    fn test_new_document() {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let doc = BlockDocument::new(ctx, agent);
        assert_eq!(doc.context_id(), ctx);
        assert_eq!(doc.agent_id(), agent);
        assert!(doc.is_empty());
        assert_eq!(doc.version(), 0);
    }

    #[test]
    fn test_insert_block_new_api() {
        let mut doc = test_doc();

        let id1 = doc.insert_block(
            None, None, Role::User, BlockKind::Text, "Hello!",
        ).unwrap();

        let id2 = doc.insert_block(
            Some(&id1), Some(&id1), Role::Model, BlockKind::Text, "Hi there!",
        ).unwrap();

        let blocks = doc.blocks_ordered();
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0].id, id1);
        assert_eq!(blocks[0].role, Role::User);
        assert_eq!(blocks[1].id, id2);
        assert_eq!(blocks[1].role, Role::Model);
        assert_eq!(blocks[1].parent_id, Some(id1));
    }

    #[test]
    fn test_dag_operations() {
        let mut doc = test_doc();

        let parent_id = doc.insert_block(
            None, None, Role::User, BlockKind::Text, "Question",
        ).unwrap();

        let child1 = doc.insert_block(
            Some(&parent_id), Some(&parent_id),
            Role::Model, BlockKind::Thinking, "Thinking...",
        ).unwrap();

        let child2 = doc.insert_block(
            Some(&parent_id), Some(&child1),
            Role::Model, BlockKind::Text, "Answer",
        ).unwrap();

        let children = doc.get_children(&parent_id);
        assert_eq!(children.len(), 2);
        assert!(children.contains(&child1));
        assert!(children.contains(&child2));

        let ancestors = doc.get_ancestors(&child1);
        assert_eq!(ancestors, vec![parent_id]);

        let roots = doc.get_roots();
        assert_eq!(roots, vec![parent_id]);

        assert_eq!(doc.get_depth(&parent_id), 0);
        assert_eq!(doc.get_depth(&child1), 1);
    }

    #[test]
    fn test_set_status() {
        let mut doc = test_doc();

        let id = doc.insert_block(
            None, None, Role::Model, BlockKind::ToolCall, "{}",
        ).unwrap();

        let snap = doc.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.status, Status::Done);

        doc.set_status(&id, Status::Running).unwrap();
        let snap = doc.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.status, Status::Running);

        doc.set_status(&id, Status::Error).unwrap();
        let snap = doc.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.status, Status::Error);
    }

    #[test]
    fn test_tool_call_and_result() {
        let mut doc = test_doc();

        let tool_call_id = doc.insert_tool_call(
            None, None, "read_file",
            serde_json::json!({"path": "/etc/hosts"}),
        ).unwrap();

        let snap = doc.get_block_snapshot(&tool_call_id).unwrap();
        assert_eq!(snap.kind, BlockKind::ToolCall);
        assert_eq!(snap.tool_name, Some("read_file".to_string()));

        let result_id = doc.insert_tool_result_block(
            &tool_call_id, Some(&tool_call_id),
            "127.0.0.1 localhost", false, Some(0),
        ).unwrap();

        let snap = doc.get_block_snapshot(&result_id).unwrap();
        assert_eq!(snap.kind, BlockKind::ToolResult);
        assert_eq!(snap.parent_id, Some(tool_call_id));
        assert_eq!(snap.tool_call_id, Some(tool_call_id));
        assert!(!snap.is_error);
        assert_eq!(snap.exit_code, Some(0));
    }

    #[test]
    fn test_insert_and_order() {
        let mut doc = test_doc();

        let id1 = doc.insert_block(None, None, Role::User, BlockKind::Text, "First").unwrap();
        let id2 = doc.insert_block(None, Some(&id1), Role::User, BlockKind::Text, "Second").unwrap();
        let id3 = doc.insert_block(None, Some(&id2), Role::User, BlockKind::Text, "Third").unwrap();

        let order: Vec<_> = doc.blocks_ordered().iter().map(|b| b.id).collect();
        assert_eq!(order, vec![id1, id2, id3]);
    }

    #[test]
    fn test_insert_at_beginning() {
        let mut doc = test_doc();

        let id1 = doc.insert_block(None, None, Role::User, BlockKind::Text, "First").unwrap();
        let id2 = doc.insert_block(None, None, Role::User, BlockKind::Text, "Before First").unwrap();

        let order: Vec<_> = doc.blocks_ordered().iter().map(|b| b.id).collect();
        assert_eq!(order, vec![id2, id1]);
    }

    #[test]
    fn test_snapshot_roundtrip() {
        let mut doc = test_doc();

        doc.insert_block(None, None, Role::Model, BlockKind::Thinking, "Thinking...").unwrap();
        doc.insert_block(None, None, Role::Model, BlockKind::Text, "Response").unwrap();

        let snapshot = doc.snapshot();
        let restored = BlockDocument::from_snapshot(snapshot.clone(), PrincipalId::new());

        assert_eq!(restored.block_count(), doc.block_count());
        assert_eq!(restored.full_text(), doc.full_text());
    }

    #[test]
    fn test_text_editing() {
        let mut doc = test_doc();

        let id = doc.insert_block(None, None, Role::User, BlockKind::Text, "Hello").unwrap();
        doc.append_text(&id, " World").unwrap();

        let text = doc.get_block_snapshot(&id).unwrap().content;
        assert_eq!(text, "Hello World");

        doc.edit_text(&id, 5, ",", 0).unwrap();
        let text = doc.get_block_snapshot(&id).unwrap().content;
        assert_eq!(text, "Hello, World");
    }

    #[test]
    fn test_move_block() {
        let mut doc = test_doc();

        let a = doc.insert_block(None, None, Role::User, BlockKind::Text, "A").unwrap();
        let b = doc.insert_block(None, Some(&a), Role::User, BlockKind::Text, "B").unwrap();
        let c = doc.insert_block(None, Some(&b), Role::User, BlockKind::Text, "C").unwrap();

        let ordered = doc.blocks_ordered();
        assert_eq!(ordered.len(), 3);
        assert_eq!(ordered[0].id, a);
        assert_eq!(ordered[1].id, b);
        assert_eq!(ordered[2].id, c);

        // Move C to the beginning
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
        let fake_id = BlockId::new(doc.context_id(), doc.agent_id(), 999);
        assert!(doc.move_block(&fake_id, None).is_err());
        assert!(doc.move_block(&a, Some(&fake_id)).is_err());
    }

    #[test]
    fn test_sync_fails_with_independent_blocks_set_creation() {
        let (mut server, mut client) = test_doc_pair();

        let frontier_after_init = server.frontier();

        let _block_id = server.insert_block(
            None, None, Role::User, BlockKind::Text, "Hello from server",
        ).unwrap();

        let incremental_ops = server.ops_since(&frontier_after_init);

        let result = client.merge_ops_owned(incremental_ops);

        assert!(
            result.is_err(),
            "Expected DataMissing error when merging incremental ops with independent oplog roots"
        );

        let err_msg = format!("{:?}", result.unwrap_err());
        assert!(
            err_msg.contains("DataMissing") || err_msg.contains("Missing"),
            "Expected DataMissing error, got: {}", err_msg
        );
    }

    #[test]
    fn test_sync_succeeds_with_full_oplog() {
        use diamond_types_extended::Document;

        let mut server = test_doc();

        let _block_id = server.insert_block(
            None, None, Role::User, BlockKind::Text, "Hello from server",
        ).unwrap();

        let full_ops = server.ops_since(&Frontier::root());

        let mut client_doc = Document::new();
        let result = client_doc.merge_ops(full_ops);

        assert!(result.is_ok(), "Full oplog merge should succeed: {:?}", result);
        assert!(
            client_doc.root().contains_key("blocks"),
            "Client should have 'blocks' key after merging full oplog"
        );
    }

    #[test]
    fn test_incremental_sync_after_full_sync() {
        use diamond_types_extended::Document;

        let mut server = test_doc();
        let full_ops = server.ops_since(&Frontier::root());

        let mut client_doc = Document::new();
        client_doc.merge_ops(full_ops).expect("initial sync should work");

        let frontier_before_block = server.frontier();

        let _block_id = server.insert_block(
            None, None, Role::User, BlockKind::Text, "New block",
        ).unwrap();

        let incremental_ops = server.ops_since(&frontier_before_block);
        let result = client_doc.merge_ops(incremental_ops);

        assert!(result.is_ok(), "Incremental merge after full sync should succeed: {:?}", result);
    }

    #[test]
    fn test_snapshot_then_streaming_should_work() {
        let mut server = test_doc();
        let block_id = server.insert_block(
            None, None, Role::Model, BlockKind::Text, "Initial content",
        ).unwrap();

        let oplog_bytes = server.oplog_bytes().unwrap();

        let client_agent = PrincipalId::new();
        let mut client = BlockDocument::from_oplog(server.context_id(), client_agent, &oplog_bytes)
            .expect("from_oplog should succeed");

        assert_eq!(client.block_count(), 1);

        let frontier_before_append = server.frontier();
        server.append_text(&block_id, " more text").unwrap();
        let incremental_ops = server.ops_since(&frontier_before_append);

        let result = client.merge_ops_owned(incremental_ops);

        assert!(
            result.is_ok(),
            "Merge should succeed after oplog sync. Error: {:?}",
            result.err()
        );

        let final_content = client.get_block_snapshot(&block_id).unwrap().content;
        assert_eq!(final_content, "Initial content more text");
    }

    #[test]
    fn test_text_streaming_sync() {
        use diamond_types_extended::Document;

        let mut server = test_doc();
        let block_id = server.insert_block(
            None, None, Role::Model, BlockKind::Text, "",
        ).unwrap();

        let full_ops = server.ops_since(&Frontier::root());
        let mut client_doc = Document::new();
        client_doc.merge_ops(full_ops).expect("initial sync");

        let chunks = ["Hello", " ", "World", "!"];

        for chunk in chunks {
            let frontier_before = server.frontier();
            server.append_text(&block_id, chunk).unwrap();

            let chunk_ops = server.ops_since(&frontier_before);
            client_doc.merge_ops(chunk_ops)
                .unwrap_or_else(|e| panic!("merging chunk '{}' should work: {:?}", chunk, e));
        }

        let server_content = server.get_block_snapshot(&block_id).unwrap().content;
        assert_eq!(server_content, "Hello World!");
    }

    #[test]
    fn test_fork_document() {
        let mut original = test_doc();

        let user_msg = original.insert_block(
            None, None, Role::User, BlockKind::Text, "Hello Claude!",
        ).unwrap();

        let _model_response = original.insert_block(
            Some(&user_msg), Some(&user_msg),
            Role::Model, BlockKind::Text, "Hi Amy!",
        ).unwrap();

        let fork_ctx = ContextId::new();
        let fork_agent = PrincipalId::new();
        let forked = original.fork(fork_ctx, fork_agent);

        assert_eq!(forked.block_count(), 2);
        assert_eq!(forked.context_id(), fork_ctx);
        assert_eq!(forked.agent_id(), fork_agent);

        let forked_blocks = forked.blocks_ordered();
        assert_eq!(forked_blocks[0].content, "Hello Claude!");
        assert_eq!(forked_blocks[0].role, Role::User);
        assert_eq!(forked_blocks[1].content, "Hi Amy!");
        assert_eq!(forked_blocks[1].role, Role::Model);

        // Blocks should have new context_id
        assert_eq!(forked_blocks[0].id.context_id, fork_ctx);
        assert_eq!(forked_blocks[1].id.context_id, fork_ctx);

        // Parent-child preserved with new IDs
        assert!(forked_blocks[0].parent_id.is_none());
        assert!(forked_blocks[1].parent_id.is_some());
        assert_eq!(forked_blocks[1].parent_id.unwrap().context_id, fork_ctx);

        // Original unchanged
        assert_eq!(original.block_count(), 2);
        let original_blocks = original.blocks_ordered();
        assert_eq!(original_blocks[0].id.context_id, original.context_id());

        // Editing forked doesn't affect original
        let forked_first_block = forked_blocks[0].id;
        let mut forked_mut = forked;
        forked_mut.append_text(&forked_first_block, " How are you?").unwrap();

        let forked_content = forked_mut.get_block_snapshot(&forked_first_block).unwrap().content;
        assert_eq!(forked_content, "Hello Claude! How are you?");

        let original_content = original.get_block_snapshot(&user_msg).unwrap().content;
        assert_eq!(original_content, "Hello Claude!");
    }

    #[test]
    fn test_fork_with_tool_blocks() {
        let mut original = test_doc();

        let tool_call_id = original.insert_tool_call(
            None, None, "read_file",
            serde_json::json!({"path": "/etc/hosts"}),
        ).unwrap();

        let _tool_result_id = original.insert_tool_result_block(
            &tool_call_id, Some(&tool_call_id),
            "127.0.0.1 localhost", false, Some(0),
        ).unwrap();

        let fork_ctx = ContextId::new();
        let forked = original.fork(fork_ctx, PrincipalId::new());

        assert_eq!(forked.block_count(), 2);

        let forked_blocks = forked.blocks_ordered();
        assert_eq!(forked_blocks[0].kind, BlockKind::ToolCall);
        assert_eq!(forked_blocks[0].tool_name, Some("read_file".to_string()));
        assert_eq!(forked_blocks[1].kind, BlockKind::ToolResult);
        assert_eq!(forked_blocks[1].content, "127.0.0.1 localhost");

        let tool_call_id_ref = forked_blocks[1].tool_call_id.as_ref().expect("should have tool_call_id");
        assert_eq!(tool_call_id_ref.context_id, fork_ctx);
        assert_eq!(tool_call_id_ref, &forked_blocks[0].id);
    }

    #[test]
    fn test_drift_block_roundtrip() {
        let mut doc = test_doc();
        let source_ctx = ContextId::new();

        // Create a drift block using insert_from_snapshot
        let drift_id = BlockId::new(doc.context_id(), doc.agent_id(), 0);
        let drift_snap = BlockSnapshot::drift(
            drift_id,
            None,
            "CAS has a race condition in the merge path",
            source_ctx,
            Some("claude-opus-4-6".to_string()),
            crate::DriftKind::Push,
        );

        let id = doc.insert_from_snapshot(drift_snap, None).unwrap();

        let snap = doc.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.kind, BlockKind::Drift);
        assert_eq!(snap.role, Role::System);
        assert_eq!(snap.source_context, Some(source_ctx));
        assert_eq!(snap.source_model, Some("claude-opus-4-6".to_string()));
        assert_eq!(snap.drift_kind, Some(crate::DriftKind::Push));
        assert_eq!(snap.content, "CAS has a race condition in the merge path");

        let blocks = doc.blocks_ordered();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Drift);
    }

    #[test]
    fn test_crdt_ordering_stress_100_bisections() {
        let mut doc = test_doc();

        let first = doc.insert_block(
            None, None, Role::User, BlockKind::Text, "First",
        ).unwrap();

        let _last = doc.insert_block(
            None, Some(&first), Role::User, BlockKind::Text, "Last",
        ).unwrap();

        let mut middle_ids = Vec::new();
        for i in 0..100 {
            let id = doc.insert_block(
                None, Some(&first),
                Role::User, BlockKind::Text, &format!("Middle-{}", i),
            ).unwrap();
            middle_ids.push(id);
        }

        let blocks = doc.blocks_ordered();
        assert_eq!(blocks.len(), 102, "All 102 blocks should be in the document");

        let mut order_keys = Vec::new();
        for block in &blocks {
            let order_key = doc.get_block_order_key(&block.id, "");
            order_keys.push(order_key);
        }

        let unique_count = order_keys.iter().collect::<std::collections::HashSet<_>>().len();
        assert_eq!(unique_count, 102, "All 102 order keys should be unique");

        for i in 1..order_keys.len() {
            assert!(
                order_keys[i - 1] < order_keys[i],
                "Order keys should be strictly sorted: {:?} >= {:?} at index {}",
                order_keys[i - 1], order_keys[i], i
            );
        }

        let block_ids: Vec<_> = blocks.iter().map(|b| &b.id).collect();
        assert_eq!(block_ids[0], &first, "First block should remain first");

        for id in &middle_ids {
            assert!(
                block_ids.contains(&id),
                "All inserted blocks should still be present in blocks_ordered()"
            );
        }
    }

    #[test]
    fn test_fork_preserves_drift_metadata() {
        let mut original = test_doc();
        let source_ctx = ContextId::new();

        let user_msg = original.insert_block(
            None, None, Role::User, BlockKind::Text, "Hello!",
        ).unwrap();

        let drift_id = BlockId::new(original.context_id(), original.agent_id(), 99);
        let drift_snap = BlockSnapshot::drift(
            drift_id,
            Some(user_msg),
            "Summary from gemini context",
            source_ctx,
            Some("gemini-2.0-flash".to_string()),
            crate::DriftKind::Distill,
        );

        original.insert_from_snapshot(drift_snap, Some(&user_msg)).unwrap();

        let fork_ctx = ContextId::new();
        let forked = original.fork(fork_ctx, PrincipalId::new());
        assert_eq!(forked.block_count(), 2);

        let blocks = forked.blocks_ordered();
        let drift_block = &blocks[1];
        assert_eq!(drift_block.kind, BlockKind::Drift);
        assert_eq!(drift_block.source_context, Some(source_ctx));
        assert_eq!(drift_block.source_model, Some("gemini-2.0-flash".to_string()));
        assert_eq!(drift_block.drift_kind, Some(crate::DriftKind::Distill));
        assert_eq!(drift_block.content, "Summary from gemini context");
    }

    #[test]
    fn test_get_block_snapshot_missing_kind() {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let mut doc = BlockDocument::new(ctx, agent);

        // Insert a block via raw transaction, skipping "kind"
        let block_id = BlockId::new(ctx, agent, 0);
        let block_key = block_id.to_key();
        doc.doc.transact(doc.agent, |tx| {
            if let Some(mut blocks_set) = tx.get_set_mut(&["blocks"]) {
                blocks_set.add_str(&block_key);
            }
            tx.root().set(&format!("order:{}", block_key), "V");
            let map_id = tx.root().create_map(&format!("block:{}", block_key));
            let mut block_map = tx.map_by_id(map_id);
            block_map.set("role", "human");
            block_map.set("status", "done");
            block_map.create_text("content");
        });
        doc.next_seq = 1;

        assert!(doc.get_block_snapshot(&block_id).is_none(),
            "Block missing 'kind' should return None");
        assert_eq!(doc.blocks_ordered().len(), 0);
    }

    #[test]
    fn test_get_block_snapshot_missing_role() {
        let ctx = ContextId::new();
        let agent = PrincipalId::new();
        let mut doc = BlockDocument::new(ctx, agent);

        let block_id = BlockId::new(ctx, agent, 0);
        let block_key = block_id.to_key();
        doc.doc.transact(doc.agent, |tx| {
            if let Some(mut blocks_set) = tx.get_set_mut(&["blocks"]) {
                blocks_set.add_str(&block_key);
            }
            tx.root().set(&format!("order:{}", block_key), "V");
            let map_id = tx.root().create_map(&format!("block:{}", block_key));
            let mut block_map = tx.map_by_id(map_id);
            block_map.set("kind", "text");
            block_map.set("status", "done");
            block_map.create_text("content");
        });
        doc.next_seq = 1;

        assert!(doc.get_block_snapshot(&block_id).is_none(),
            "Block missing 'role' should return None");
    }

    #[test]
    fn test_order_midpoint_basic() {
        let mid = order_midpoint("A", "Z");
        assert!(mid > "A".to_string() && mid < "Z".to_string(),
            "midpoint {} should be between A and Z", mid);

        let mid2 = order_midpoint("", "V");
        assert!(mid2 < "V".to_string(),
            "midpoint {} should be before V", mid2);

        let mid3 = order_midpoint("A", "B");
        assert!(mid3 > "A".to_string() && mid3 < "B".to_string(),
            "midpoint {} should be between A and B", mid3);
    }

    #[test]
    fn test_oplog_bytes_returns_result() {
        let doc = test_doc();
        let bytes = doc.oplog_bytes();
        assert!(bytes.is_ok(), "oplog_bytes should succeed for valid document");
        assert!(!bytes.unwrap().is_empty());
    }

    #[test]
    fn test_compact_preserves_blocks() {
        let mut doc = test_doc();

        let id1 = doc.insert_block(None, None, Role::User, BlockKind::Text, "Hello!").unwrap();
        let id2 = doc.insert_block(Some(&id1), Some(&id1), Role::Model, BlockKind::Text, "Hi there!").unwrap();
        let id3 = doc.insert_block(Some(&id2), Some(&id2), Role::Model, BlockKind::Thinking, "Let me think...").unwrap();

        doc.edit_text(&id1, 6, " World", 0).unwrap();
        doc.edit_text(&id2, 9, " How are you?", 0).unwrap();
        doc.set_status(&id3, Status::Done).unwrap();

        let blocks_before = doc.blocks_ordered();
        let oplog_before = doc.oplog_bytes().unwrap().len();

        let new_frontier = doc.compact();
        assert!(new_frontier.is_ok());

        let blocks_after = doc.blocks_ordered();
        let oplog_after = doc.oplog_bytes().unwrap().len();

        assert_eq!(blocks_before.len(), blocks_after.len());
        for (before, after) in blocks_before.iter().zip(blocks_after.iter()) {
            assert_eq!(before.id, after.id);
            assert_eq!(before.content, after.content);
            assert_eq!(before.role, after.role);
            assert_eq!(before.kind, after.kind);
            assert_eq!(before.status, after.status);
        }

        assert!(oplog_after <= oplog_before, "oplog should not grow: {} vs {}", oplog_after, oplog_before);
    }

    #[test]
    fn test_compact_then_new_ops() {
        let ctx = ContextId::new();
        let server_agent = PrincipalId::new();
        let mut doc = BlockDocument::new(ctx, server_agent);

        let id1 = doc.insert_block(None, None, Role::User, BlockKind::Text, "First").unwrap();
        doc.edit_text(&id1, 5, " message", 0).unwrap();

        doc.compact().unwrap();

        let client_agent = PrincipalId::new();
        let mut client = BlockDocument::new_for_sync(ctx, client_agent);
        let server_ops = doc.ops_since(&Frontier::root());
        client.merge_ops_owned(server_ops).unwrap();

        let client_blocks = client.blocks_ordered();
        assert_eq!(client_blocks.len(), 1);
        assert_eq!(client_blocks[0].content, "First message");

        let client_id = client.insert_block(Some(&id1), Some(&id1), Role::Model, BlockKind::Text, "Reply").unwrap();
        let client_ops = client.ops_since(&doc.frontier());
        doc.merge_ops_owned(client_ops).unwrap();

        let server_blocks = doc.blocks_ordered();
        assert_eq!(server_blocks.len(), 2);
        assert_eq!(server_blocks[1].id, client_id);
    }

    #[test]
    fn test_compact_reduction_with_overwrites() {
        let mut doc = test_doc();

        let mut ids = Vec::new();
        let mut last_id = None;
        for _i in 0..10 {
            let role = if ids.len() % 2 == 0 { Role::User } else { Role::Model };
            let id = doc.insert_block(
                last_id.as_ref(), last_id.as_ref(),
                role, BlockKind::Text, "initial content here",
            ).unwrap();
            ids.push(id);
            last_id = Some(id);
        }

        for id in &ids {
            for round in 0..5 {
                let current = doc.get_block_snapshot(id).unwrap().content;
                let len = current.len();
                doc.edit_text(id, 0, "", len).unwrap();
                doc.edit_text(id, 0, &format!("rewritten content round {} for block", round), 0).unwrap();
            }
            doc.set_status(id, Status::Running).unwrap();
            doc.set_status(id, Status::Error).unwrap();
            doc.set_status(id, Status::Done).unwrap();
        }

        let oplog_before = doc.oplog_bytes().unwrap().len();
        let blocks_before = doc.blocks_ordered();

        doc.compact().unwrap();

        let oplog_after = doc.oplog_bytes().unwrap().len();
        let blocks_after = doc.blocks_ordered();

        assert_eq!(blocks_before.len(), blocks_after.len());
        for (before, after) in blocks_before.iter().zip(blocks_after.iter()) {
            assert_eq!(before.content, after.content);
        }
        assert!(oplog_after < oplog_before,
            "compaction should reduce oplog: {} -> {}", oplog_before, oplog_after);
    }

    #[test]
    fn test_promote_to_register() {
        let mut doc = test_doc();

        let id = doc.insert_block(None, None, Role::Model, BlockKind::Text, "Hello World").unwrap();
        doc.promote_to_register(&id).unwrap();

        let snap = doc.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.content, "Hello World");
    }

    #[test]
    fn test_promote_roundtrip_through_snapshot() {
        let mut doc = test_doc();

        let id1 = doc.insert_block(None, None, Role::Model, BlockKind::Text, "Done block").unwrap();
        doc.set_status(&id1, Status::Done).unwrap();
        doc.promote_to_register(&id1).unwrap();

        let id2 = doc.insert_block(None, Some(&id1), Role::Model, BlockKind::Text, "Running block").unwrap();
        doc.set_status(&id2, Status::Running).unwrap();

        let snapshot = doc.snapshot();
        let restored = BlockDocument::from_snapshot(snapshot, doc.agent_id());

        let snap1 = restored.get_block_snapshot(&id1).unwrap();
        assert_eq!(snap1.content, "Done block");

        let snap2 = restored.get_block_snapshot(&id2).unwrap();
        assert_eq!(snap2.content, "Running block");
    }

    #[test]
    fn test_running_blocks_not_promoted() {
        let mut doc = test_doc();

        let id = doc.insert_block(None, None, Role::Model, BlockKind::Text, "Streaming...").unwrap();
        doc.set_status(&id, Status::Running).unwrap();

        let snapshot = doc.snapshot();
        let restored = BlockDocument::from_snapshot(snapshot, doc.agent_id());

        let snap = restored.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.content, "Streaming...");
    }

    #[test]
    fn test_compaction_promotes_done_blocks() {
        let mut doc = test_doc();

        let id1 = doc.insert_block(None, None, Role::User, BlockKind::Text, "Done content").unwrap();
        let id2 = doc.insert_block(None, Some(&id1), Role::Model, BlockKind::Text, "Still streaming").unwrap();
        doc.set_status(&id2, Status::Running).unwrap();

        doc.compact().unwrap();

        let snap1 = doc.get_block_snapshot(&id1).unwrap();
        assert_eq!(snap1.content, "Done content");

        let snap2 = doc.get_block_snapshot(&id2).unwrap();
        assert_eq!(snap2.content, "Still streaming");
    }
}
