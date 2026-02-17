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

use crate::{BlockId, BlockKind, BlockSnapshot, CrdtError, Result, Role, Status};

/// Convert a string agent ID to a deterministic UUID.
///
/// Uses a simple hash-to-bytes approach so the same agent string always
/// produces the same UUID. This is needed because diamond-types-extended
/// v0.2 changed `create_agent` to take `Uuid` instead of `&str`.
pub(crate) fn agent_uuid(agent_id: &str) -> Uuid {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    agent_id.hash(&mut hasher);
    let h1 = hasher.finish();
    // Hash again for the second 8 bytes
    h1.hash(&mut hasher);
    let h2 = hasher.finish();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&h1.to_le_bytes());
    bytes[8..].copy_from_slice(&h2.to_le_bytes());
    Uuid::from_bytes(bytes)
}

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
/// All operations go through the diamond-types-extended Document, ensuring:
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
        let agent = doc.create_agent(agent_uuid(&agent_id_str));

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
        let agent = doc.create_agent(agent_uuid(&agent_id_str));

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

        // Collect keys first, then drop the Set iterator before querying root.
        // The Set iterator borrows internal document state; querying root().get()
        // needs the same state — holding both causes a re-entrant lock deadlock
        // with DTE v0.2's interior mutability.
        let keys: Vec<String> = blocks_set
            .iter()
            .filter_map(|v| v.as_str().map(|s| s.to_string()))
            .collect();
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

        // Drift-specific fields
        let source_context = block_map.get("source_context")
            .and_then(|v| v.as_str().map(|s| s.to_string()));

        let source_model = block_map.get("source_model")
            .and_then(|v| v.as_str().map(|s| s.to_string()));

        let drift_kind = block_map.get("drift_kind")
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .and_then(|s| crate::block::DriftKind::from_str(&s));

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
        author: impl Into<String>,
    ) -> Result<BlockId> {
        let id = self.new_block_id();
        let input_json = serde_json::to_string_pretty(&tool_input)
            .map_err(|e| CrdtError::Serialization(e.to_string()))?;

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
        // Generate a local ID if the snapshot has a placeholder (empty document_id).
        // This happens for drift blocks built by DriftRouter::build_drift_block().
        let block_id = if snapshot.id.document_id.is_empty() {
            self.new_block_id()
        } else {
            // Update next_seq if needed to avoid collisions with remote IDs
            if snapshot.id.agent_id == self.agent_id_str {
                self.next_seq = self.next_seq.max(snapshot.id.seq + 1);
            }
            snapshot.id.clone()
        };

        self.insert_block_with_id(
            block_id.clone(),
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
        author: String,
        tool_name: Option<String>,
        tool_input: Option<serde_json::Value>,
        tool_call_id: Option<BlockId>,
        exit_code: Option<i32>,
        is_error: bool,
        display_hint: Option<String>,
        source_context: Option<String>,
        source_model: Option<String>,
        drift_kind: Option<crate::block::DriftKind>,
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

                // Drift-specific fields
                if let Some(ref ctx) = source_context {
                    block_map.set("source_context", ctx.as_str());
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
            if !content.is_empty() {
                if let Some(mut text) = tx.text_by_id(text_id) {
                    text.insert(0, &content);
                }
            }

            if let Some(ref input) = tool_input {
                if let Some(input_id) = tool_input_id {
                    // Note: unwrap is safe here because serde_json::Value always serializes
                    let input_json = serde_json::to_string(input).unwrap_or_else(|_| "{}".to_string());
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
            drop(blocks_set);
            for key in keys {
                if let Some(block_id) = BlockId::from_key(&key) {
                    if block_id.agent_id == self.agent_id_str {
                        self.next_seq = self.next_seq.max(block_id.seq + 1);
                    }
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

    /// Fork the document, creating a copy with a new document ID.
    ///
    /// All blocks and their content are copied to the new document.
    /// The new document gets a fresh agent ID for future edits.
    ///
    /// # Arguments
    ///
    /// * `new_doc_id` - Document ID for the forked document
    /// * `new_agent_id` - Agent ID for local operations on the fork
    ///
    /// # Returns
    ///
    /// A new BlockDocument with all blocks copied from this document.
    pub fn fork(&self, new_doc_id: &str, new_agent_id: &str) -> Self {
        // Create new document with fresh CRDT state
        let mut forked = BlockDocument::new(new_doc_id, new_agent_id);

        // Copy all blocks in order
        let blocks = self.blocks_ordered();
        let mut id_mapping: std::collections::HashMap<BlockId, BlockId> = std::collections::HashMap::new();
        let mut last_id: Option<BlockId> = None;

        for block in blocks {
            // Generate new block ID in the forked document
            let new_id = forked.new_block_id();

            // Map old ID to new ID for parent_id translation
            id_mapping.insert(block.id.clone(), new_id.clone());

            // Translate parent_id if it exists
            let new_parent_id = block.parent_id.as_ref()
                .and_then(|old_pid| id_mapping.get(old_pid).cloned());

            // Translate tool_call_id if it exists
            let new_tool_call_id = block.tool_call_id.as_ref()
                .and_then(|old_tcid| id_mapping.get(old_tcid).cloned());

            // Insert the block with translated IDs, maintaining order by inserting after last block
            let _ = forked.insert_block_with_id(
                new_id.clone(),
                new_parent_id.as_ref(),
                last_id.as_ref(), // Insert after last block to maintain order
                block.role,
                block.kind,
                block.content,
                block.author,
                block.tool_name,
                block.tool_input,
                new_tool_call_id,
                block.exit_code,
                block.is_error,
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
    ///
    /// This creates a new document containing only blocks that existed at the given version.
    ///
    /// # Arguments
    ///
    /// * `new_doc_id` - Document ID for the forked document
    /// * `new_agent_id` - Agent ID for local operations on the fork
    /// * `at_version` - Only include blocks with created_at <= this version
    ///
    /// # Returns
    ///
    /// A new BlockDocument with blocks up to the specified version.
    pub fn fork_at_version(&self, new_doc_id: &str, new_agent_id: &str, at_version: u64) -> Self {
        // Create new document with fresh CRDT state
        let mut forked = BlockDocument::new(new_doc_id, new_agent_id);

        // Get all blocks ordered, filter by version
        let blocks: Vec<_> = self.blocks_ordered()
            .into_iter()
            .filter(|b| b.created_at <= at_version)
            .collect();

        let mut id_mapping: std::collections::HashMap<BlockId, BlockId> = std::collections::HashMap::new();
        let mut last_id: Option<BlockId> = None;

        for block in blocks {
            // Generate new block ID in the forked document
            let new_id = forked.new_block_id();

            // Map old ID to new ID for parent_id translation
            id_mapping.insert(block.id.clone(), new_id.clone());

            // Translate parent_id if it exists and was included
            let new_parent_id = block.parent_id.as_ref()
                .and_then(|old_pid| id_mapping.get(old_pid).cloned());

            // Translate tool_call_id if it exists and was included
            let new_tool_call_id = block.tool_call_id.as_ref()
                .and_then(|old_tcid| id_mapping.get(old_tcid).cloned());

            // Insert the block with translated IDs, maintaining order by inserting after last block
            let _ = forked.insert_block_with_id(
                new_id.clone(),
                new_parent_id.as_ref(),
                last_id.as_ref(), // Insert after last block to maintain order
                block.role,
                block.kind,
                block.content,
                block.author,
                block.tool_name,
                block.tool_input,
                new_tool_call_id,
                block.exit_code,
                block.is_error,
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
    ///
    /// This serializes the complete oplog from empty frontier, enabling clients
    /// to receive the full CRDT history including the root "blocks" Set creation.
    /// This is essential for proper sync - clients cannot merge incremental ops
    /// without having the oplog root operations.
    pub fn oplog_bytes(&self) -> Result<Vec<u8>> {
        let ops = self.ops_since(&Frontier::root());
        serde_json::to_vec(&ops)
            .map_err(|e| CrdtError::Serialization(e.to_string()))
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

        let agent = doc.create_agent(agent_uuid(&agent_id_str));

        // Calculate next_seq from existing blocks (avoid ID collisions).
        // Collect keys first, then drop — same defensive pattern as block_ids_ordered().
        let mut next_seq = 0u64;
        if let Some(blocks_set) = doc.get_set(&["blocks"]) {
            let keys: Vec<String> = blocks_set
                .iter()
                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                .collect();
            drop(blocks_set);
            for key in keys {
                if let Some(block_id) = BlockId::from_key(&key) {
                    if block_id.agent_id == agent_id_str {
                        next_seq = next_seq.max(block_id.seq + 1);
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

    /// Compact the oplog by rebuilding from a snapshot of current state.
    ///
    /// This replaces the internal DTE Document with a fresh one containing only
    /// the operations needed to represent current block state — no edit history,
    /// no deleted block tombstones beyond what the snapshot captures.
    ///
    /// Returns the new frontier after compaction, or an error if something went wrong.
    ///
    /// **Warning:** Any connected client holding a pre-compaction frontier will get
    /// DataMissing on the next incremental op. Callers must handle re-sync.
    pub fn compact(&mut self) -> Result<Frontier> {
        let snapshot = self.snapshot();
        let compacted = Self::from_snapshot(snapshot, &self.agent_id_str);
        self.doc = compacted.doc;
        self.agent = compacted.agent;
        self.next_seq = compacted.next_seq;
        // version stays the same — compaction doesn't change logical version
        Ok(self.frontier())
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
                block_snap.source_context.clone(),
                block_snap.source_model.clone(),
                block_snap.drift_kind.clone(),
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
        use diamond_types_extended::Document;

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
        let full_ops = server.ops_since(&Frontier::root());

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
        use diamond_types_extended::Document;

        // === Initial full sync ===
        let mut server = BlockDocument::new("doc-1", "server-agent");
        let full_ops = server.ops_since(&Frontier::root());

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
        let oplog_bytes = server.oplog_bytes().unwrap();

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
        use diamond_types_extended::Document;

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
        let full_ops = server.ops_since(&Frontier::root());
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

    /// Test document fork creates a deep copy with new IDs.
    #[test]
    fn test_fork_document() {
        let mut original = BlockDocument::new("original-doc", "alice");

        // Insert some blocks
        let user_msg = original.insert_block(
            None,
            None,
            Role::User,
            BlockKind::Text,
            "Hello Claude!",
            "user:amy",
        ).unwrap();

        let _model_response = original.insert_block(
            Some(&user_msg),
            Some(&user_msg),
            Role::Model,
            BlockKind::Text,
            "Hi Amy!",
            "model:claude",
        ).unwrap();

        // Fork the document
        let forked = original.fork("forked-doc", "bob");

        // Verify forked document has same number of blocks
        assert_eq!(forked.block_count(), 2);

        // Verify forked document has different ID
        assert_eq!(forked.document_id(), "forked-doc");
        assert_eq!(forked.agent_id(), "bob");

        // Verify block content is preserved
        let forked_blocks = forked.blocks_ordered();
        assert_eq!(forked_blocks[0].content, "Hello Claude!");
        assert_eq!(forked_blocks[0].role, Role::User);
        assert_eq!(forked_blocks[1].content, "Hi Amy!");
        assert_eq!(forked_blocks[1].role, Role::Model);

        // Verify blocks have new document ID
        assert_eq!(forked_blocks[0].id.document_id, "forked-doc");
        assert_eq!(forked_blocks[1].id.document_id, "forked-doc");

        // Verify parent-child relationship is preserved
        assert!(forked_blocks[0].parent_id.is_none());
        assert!(forked_blocks[1].parent_id.is_some());
        assert_eq!(forked_blocks[1].parent_id.as_ref().unwrap().document_id, "forked-doc");

        // Verify original document is unchanged
        assert_eq!(original.block_count(), 2);
        assert_eq!(original.document_id(), "original-doc");
        let original_blocks = original.blocks_ordered();
        assert_eq!(original_blocks[0].id.document_id, "original-doc");

        // Verify editing forked document doesn't affect original
        let forked_first_block = &forked_blocks[0].id;
        let mut forked_mut = forked;
        forked_mut.append_text(forked_first_block, " How are you?").unwrap();

        let forked_content = forked_mut.get_block_snapshot(forked_first_block).unwrap().content;
        assert_eq!(forked_content, "Hello Claude! How are you?");

        let original_content = original.get_block_snapshot(&user_msg).unwrap().content;
        assert_eq!(original_content, "Hello Claude!");
    }

    /// Test fork with tool call blocks preserves tool_call_id references.
    #[test]
    fn test_fork_with_tool_blocks() {
        let mut original = BlockDocument::new("original-doc", "server");

        // Insert a tool call
        let tool_call_id = original.insert_tool_call(
            None,
            None,
            "read_file",
            serde_json::json!({"path": "/etc/hosts"}),
            "model:claude",
        ).unwrap();

        // Insert tool result
        let _tool_result_id = original.insert_tool_result_block(
            &tool_call_id,
            Some(&tool_call_id),
            "127.0.0.1 localhost",
            false,
            Some(0),
            "system",
        ).unwrap();

        // Fork the document
        let forked = original.fork("forked-doc", "client");

        // Verify blocks exist
        assert_eq!(forked.block_count(), 2);

        let forked_blocks = forked.blocks_ordered();

        // Verify tool call
        assert_eq!(forked_blocks[0].kind, BlockKind::ToolCall);
        assert_eq!(forked_blocks[0].tool_name, Some("read_file".to_string()));

        // Verify tool result
        assert_eq!(forked_blocks[1].kind, BlockKind::ToolResult);
        assert_eq!(forked_blocks[1].content, "127.0.0.1 localhost");

        // Verify tool_call_id reference is properly translated
        let tool_call_id_ref = forked_blocks[1].tool_call_id.as_ref().expect("should have tool_call_id");
        assert_eq!(tool_call_id_ref.document_id, "forked-doc");
        assert_eq!(tool_call_id_ref, &forked_blocks[0].id);
    }

    /// Test drift block insertion, CRDT round-trip, and metadata preservation.
    #[test]
    fn test_drift_block_roundtrip() {
        use crate::block::DriftKind;

        let mut doc = BlockDocument::new("doc-1", "server");

        // Create a drift block using insert_from_snapshot
        let drift_snap = BlockSnapshot::drift(
            BlockId::new("doc-1", "drift", 0),
            None,
            "CAS has a race condition in the merge path",
            "drift:a1b2c3",
            "a1b2c3",
            Some("claude-opus-4-6".to_string()),
            DriftKind::Push,
        );

        let id = doc.insert_from_snapshot(drift_snap, None).unwrap();

        // Read it back from the CRDT
        let snap = doc.get_block_snapshot(&id).unwrap();
        assert_eq!(snap.kind, BlockKind::Drift);
        assert_eq!(snap.role, Role::System);
        assert_eq!(snap.source_context, Some("a1b2c3".to_string()));
        assert_eq!(snap.source_model, Some("claude-opus-4-6".to_string()));
        assert_eq!(snap.drift_kind, Some(DriftKind::Push));
        assert_eq!(snap.content, "CAS has a race condition in the merge path");
        assert_eq!(snap.author, "drift:a1b2c3");

        // Verify it shows up in blocks_ordered
        let blocks = doc.blocks_ordered();
        assert_eq!(blocks.len(), 1);
        assert_eq!(blocks[0].kind, BlockKind::Drift);
    }

    /// Test 4b: CRDT ordering stress test — 100 bisections between same siblings.
    ///
    /// This test demonstrates the precision limits of the current f64-based ordering.
    /// After ~40 bisections, floating point precision loss causes order values to
    /// collide when round-tripped through i64 storage (scaled by 1e12).
    ///
    /// This validates the TODO comment in calc_order_key about migrating to
    /// string-based fractional indexing for unlimited precision.
    #[test]
    fn test_crdt_ordering_stress_100_bisections() {
        let mut doc = BlockDocument::new("doc-1", "alice");

        // Create two sentinel blocks to bisect between
        let first = doc.insert_block(
            None, None,
            Role::User, BlockKind::Text,
            "First", "alice"
        ).unwrap();

        let _last = doc.insert_block(
            None, Some(&first),
            Role::User, BlockKind::Text,
            "Last", "alice"
        ).unwrap();

        // Insert 100 blocks between first and last (repeated bisection)
        let mut middle_ids = Vec::new();
        for i in 0..100 {
            let id = doc.insert_block(
                None, Some(&first), // Always insert after 'first'
                Role::User, BlockKind::Text,
                &format!("Middle-{}", i), "alice"
            ).unwrap();
            middle_ids.push(id);
        }

        // All blocks should exist in document
        let blocks = doc.blocks_ordered();
        assert_eq!(blocks.len(), 102, "All 102 blocks should be in the document");

        // Collect order keys and verify ALL are unique (string-based = unlimited precision)
        let mut order_keys = Vec::new();
        for block in &blocks {
            let order_key = doc.get_block_order_key(&block.id, "");
            order_keys.push(order_key);
        }

        let unique_count = order_keys.iter().collect::<std::collections::HashSet<_>>().len();

        // String-based fractional indexing guarantees all 102 values are unique
        assert_eq!(
            unique_count, 102,
            "All 102 order keys should be unique with string-based indexing, got {}",
            unique_count
        );

        // Verify ordering is strictly sorted
        for i in 1..order_keys.len() {
            assert!(
                order_keys[i - 1] < order_keys[i],
                "Order keys should be strictly sorted: {:?} >= {:?} at index {}",
                order_keys[i - 1], order_keys[i], i
            );
        }

        let block_ids: Vec<_> = blocks.iter().map(|b| &b.id).collect();

        // First should be the first sentinel
        assert_eq!(block_ids[0], &first, "First block should remain first");

        // All inserted blocks should be present
        for id in &middle_ids {
            assert!(
                block_ids.contains(&id),
                "All inserted blocks should still be present in blocks_ordered()"
            );
        }
    }

    /// Test that drift blocks survive fork (metadata preserved with new IDs).
    #[test]
    fn test_fork_preserves_drift_metadata() {
        use crate::block::DriftKind;

        let mut original = BlockDocument::new("original", "server");

        // Add a regular block first
        let user_msg = original.insert_block(
            None, None,
            Role::User, BlockKind::Text,
            "Hello!", "user:amy",
        ).unwrap();

        // Add a drift block
        let drift_snap = BlockSnapshot::drift(
            BlockId::new("original", "drift", 0),
            Some(user_msg.clone()),
            "Summary from gemini context",
            "drift:d4e5f6",
            "d4e5f6",
            Some("gemini-2.0-flash".to_string()),
            DriftKind::Distill,
        );
        original.insert_from_snapshot(drift_snap, Some(&user_msg)).unwrap();

        // Fork it
        let forked = original.fork("forked", "client");
        assert_eq!(forked.block_count(), 2);

        let blocks = forked.blocks_ordered();
        let drift_block = &blocks[1];
        assert_eq!(drift_block.kind, BlockKind::Drift);
        assert_eq!(drift_block.source_context, Some("d4e5f6".to_string()));
        assert_eq!(drift_block.source_model, Some("gemini-2.0-flash".to_string()));
        assert_eq!(drift_block.drift_kind, Some(DriftKind::Distill));
        assert_eq!(drift_block.content, "Summary from gemini context");
    }

    /// Test that get_block_snapshot returns None for blocks missing 'kind' field.
    #[test]
    fn test_get_block_snapshot_missing_kind() {
        let mut doc = BlockDocument::new("doc-1", "alice");

        // Insert a block via raw transaction, skipping "kind"
        let block_key = "doc-1/alice/0";
        doc.doc.transact(doc.agent, |tx| {
            if let Some(mut blocks_set) = tx.get_set_mut(&["blocks"]) {
                blocks_set.add_str(block_key);
            }
            tx.root().set(&format!("order:{}", block_key), "V");
            let map_id = tx.root().create_map(&format!("block:{}", block_key));
            let mut block_map = tx.map_by_id(map_id);
            // Set role but NOT kind
            block_map.set("role", "human");
            block_map.set("status", "done");
            block_map.set("author", "test");
            block_map.create_text("content");
        });
        doc.next_seq = 1;

        let block_id = BlockId::from_key(block_key).unwrap();
        assert!(doc.get_block_snapshot(&block_id).is_none(),
            "Block missing 'kind' should return None");

        // blocks_ordered should also skip it
        assert_eq!(doc.blocks_ordered().len(), 0);
    }

    /// Test that get_block_snapshot returns None for blocks missing 'role' field.
    #[test]
    fn test_get_block_snapshot_missing_role() {
        let mut doc = BlockDocument::new("doc-1", "alice");

        let block_key = "doc-1/alice/0";
        doc.doc.transact(doc.agent, |tx| {
            if let Some(mut blocks_set) = tx.get_set_mut(&["blocks"]) {
                blocks_set.add_str(block_key);
            }
            tx.root().set(&format!("order:{}", block_key), "V");
            let map_id = tx.root().create_map(&format!("block:{}", block_key));
            let mut block_map = tx.map_by_id(map_id);
            // Set kind but NOT role
            block_map.set("kind", "text");
            block_map.set("status", "done");
            block_map.set("author", "test");
            block_map.create_text("content");
        });
        doc.next_seq = 1;

        let block_id = BlockId::from_key(block_key).unwrap();
        assert!(doc.get_block_snapshot(&block_id).is_none(),
            "Block missing 'role' should return None");
    }

    /// Test that order_midpoint produces correct lexicographic midpoints.
    #[test]
    fn test_order_midpoint_basic() {
        let mid = order_midpoint("A", "Z");
        assert!(mid > "A".to_string() && mid < "Z".to_string(),
            "midpoint {} should be between A and Z", mid);

        let mid2 = order_midpoint("", "V");
        assert!(mid2 < "V".to_string(),
            "midpoint {} should be before V", mid2);

        // Adjacent characters
        let mid3 = order_midpoint("A", "B");
        assert!(mid3 > "A".to_string() && mid3 < "B".to_string(),
            "midpoint {} should be between A and B", mid3);
    }

    /// Test oplog_bytes returns Result.
    #[test]
    fn test_oplog_bytes_returns_result() {
        let doc = BlockDocument::new("doc-1", "alice");
        let bytes = doc.oplog_bytes();
        assert!(bytes.is_ok(), "oplog_bytes should succeed for valid document");
        assert!(!bytes.unwrap().is_empty());
    }

    #[test]
    fn test_compact_preserves_blocks() {
        let mut doc = BlockDocument::new("doc-1", "alice");

        // Add several blocks with content
        let id1 = doc.insert_block(None, None, Role::User, BlockKind::Text, "Hello!", "user:amy").unwrap();
        let id2 = doc.insert_block(Some(&id1), Some(&id1), Role::Model, BlockKind::Text, "Hi there!", "model:claude").unwrap();
        let id3 = doc.insert_block(Some(&id2), Some(&id2), Role::Model, BlockKind::Thinking, "Let me think...", "model:claude").unwrap();

        // Edit content to grow the oplog
        doc.edit_text(&id1, 6, " World", 0).unwrap();
        doc.edit_text(&id2, 9, " How are you?", 0).unwrap();
        doc.set_status(&id3, Status::Done).unwrap();

        let blocks_before = doc.blocks_ordered();
        let oplog_before = doc.oplog_bytes().unwrap().len();

        // Compact
        let new_frontier = doc.compact();
        assert!(new_frontier.is_ok());

        let blocks_after = doc.blocks_ordered();
        let oplog_after = doc.oplog_bytes().unwrap().len();

        // Blocks should be identical
        assert_eq!(blocks_before.len(), blocks_after.len());
        for (before, after) in blocks_before.iter().zip(blocks_after.iter()) {
            assert_eq!(before.id, after.id);
            assert_eq!(before.content, after.content);
            assert_eq!(before.role, after.role);
            assert_eq!(before.kind, after.kind);
            assert_eq!(before.status, after.status);
        }

        // Oplog should be smaller (no edit history, just reconstructed state)
        assert!(oplog_after <= oplog_before, "oplog should not grow: {} vs {}", oplog_after, oplog_before);
    }

    #[test]
    fn test_compact_then_new_ops() {
        let mut doc = BlockDocument::new("doc-1", "server");

        let id1 = doc.insert_block(None, None, Role::User, BlockKind::Text, "First", "user:amy").unwrap();
        doc.edit_text(&id1, 5, " message", 0).unwrap();

        // Compact the server doc
        doc.compact().unwrap();

        // A fresh client should be able to sync from the compacted oplog
        let mut client = BlockDocument::new_for_sync("doc-1", "client");
        let server_ops = doc.ops_since(&Frontier::root());
        client.merge_ops_owned(server_ops).unwrap();

        let client_blocks = client.blocks_ordered();
        assert_eq!(client_blocks.len(), 1);
        assert_eq!(client_blocks[0].content, "First message");

        // Client should be able to send new ops back
        let client_id = client.insert_block(Some(&id1), Some(&id1), Role::Model, BlockKind::Text, "Reply", "model:claude").unwrap();
        let client_ops = client.ops_since(&doc.frontier());
        doc.merge_ops_owned(client_ops).unwrap();

        let server_blocks = doc.blocks_ordered();
        assert_eq!(server_blocks.len(), 2);
        assert_eq!(server_blocks[1].id, client_id);
    }
}
