//! Cell components for Bevy ECS.
//!
//! Cells are the fundamental content primitive in Kaijutsu. Each cell contains
//! structured content blocks (text, thinking, tool use/results) managed by CRDTs.

use bevy::prelude::*;

// Re-export CRDT types for convenience
pub use kaijutsu_crdt::{
    Block, BlockContent, BlockContentSnapshot, BlockDocOp, BlockDocument, BlockId, BlockType,
};

/// Unique identifier for a cell.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CellId(pub String);

impl CellId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }
}

impl Default for CellId {
    fn default() -> Self {
        Self::new()
    }
}

/// The type/kind of cell content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CellKind {
    /// Executable code (language specified separately)
    #[default]
    Code,
    /// Markdown text
    Markdown,
    /// Output from execution
    Output,
    /// System message
    System,
    /// User conversation message
    UserMessage,
    /// Agent/AI conversation message
    AgentMessage,
}

/// Component linking a cell to a conversation.
///
/// When attached to a cell (like MainCell), the cell's content
/// is synced with the conversation's BlockDocument.
#[derive(Component, Debug, Clone)]
pub struct ViewingConversation {
    /// ID of the conversation this cell is viewing.
    pub conversation_id: String,
    /// Last sync version to detect changes.
    pub last_sync_version: u64,
}

/// Core cell component - the fundamental content primitive.
#[derive(Component)]
pub struct Cell {
    /// Unique identifier
    pub id: CellId,
    /// What kind of content this cell holds
    pub kind: CellKind,
    /// Programming language (for code cells)
    pub language: Option<String>,
    /// Parent cell (for nesting, e.g., tool calls under messages)
    pub parent: Option<CellId>,
}

impl Cell {
    pub fn new(kind: CellKind) -> Self {
        Self {
            id: CellId::new(),
            kind,
            language: None,
            parent: None,
        }
    }

    pub fn code(language: impl Into<String>) -> Self {
        Self {
            id: CellId::new(),
            kind: CellKind::Code,
            language: Some(language.into()),
            parent: None,
        }
    }
}

// ============================================================================
// CURSOR TYPES
// ============================================================================

/// Cursor position within a block document.
#[derive(Debug, Clone, Default)]
pub struct BlockCursor {
    /// Which block the cursor is in.
    pub block_id: Option<BlockId>,
    /// Character offset within the block.
    pub offset: usize,
}

impl BlockCursor {
    /// Create a cursor at a specific position.
    pub fn at(block_id: BlockId, offset: usize) -> Self {
        Self {
            block_id: Some(block_id),
            offset,
        }
    }
}

// ============================================================================
// CELL EDITOR COMPONENT
// ============================================================================

/// Text editor state for a cell.
///
/// The `doc` field (BlockDocument) is the single source of truth for all content.
/// All modifications go through the BlockDocument's CRDT operations.
#[derive(Component)]
pub struct CellEditor {
    /// Block document - the source of truth for all content.
    pub doc: BlockDocument,

    /// Cursor position within the document.
    pub cursor: BlockCursor,

    /// Whether content has changed since last sync.
    pub dirty: bool,
}

impl Default for CellEditor {
    fn default() -> Self {
        Self::new()
    }
}

impl CellEditor {
    /// Create a new editor with a random agent ID.
    pub fn new() -> Self {
        let agent_id = uuid::Uuid::new_v4().to_string();
        let cell_id = uuid::Uuid::new_v4().to_string();
        Self {
            doc: BlockDocument::new(&cell_id, &agent_id),
            cursor: BlockCursor::default(),
            dirty: false,
        }
    }

    /// Builder: set initial text content (creates a single text block).
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        if !text.is_empty() {
            if let Ok(block_id) = self.doc.insert_text_block(None, &text) {
                self.cursor = BlockCursor::at(block_id, text.len());
            }
        }
        self
    }

    // =========================================================================
    // TEXT ACCESS
    // =========================================================================

    /// Get the full text content (concatenation of all blocks).
    pub fn text(&self) -> String {
        self.doc.full_text()
    }

    /// Get the current document version.
    pub fn version(&self) -> u64 {
        self.doc.version()
    }

    /// Check if the editor has any blocks.
    pub fn has_blocks(&self) -> bool {
        !self.doc.is_empty()
    }

    /// Get blocks in order.
    pub fn blocks(&self) -> Vec<&Block> {
        self.doc.blocks_ordered()
    }

    // =========================================================================
    // TEXT MUTATION
    // =========================================================================

    /// Apply server-authoritative content (doesn't generate ops).
    pub fn apply_server_content(&mut self, content: impl Into<String>) {
        // For server content, we set text without marking dirty
        // because this is already synced.
        let text = content.into();

        // Clear existing blocks
        let block_ids: Vec<_> = self
            .doc
            .blocks_ordered()
            .iter()
            .map(|b| b.id.clone())
            .collect();
        for id in block_ids {
            let _ = self.doc.delete_block(&id);
        }

        // Take any pending ops (discard them - server is authoritative)
        let _ = self.doc.take_pending_ops();

        // Create new text block
        if !text.is_empty() {
            if let Ok(block_id) = self.doc.insert_text_block(None, &text) {
                self.cursor = BlockCursor::at(block_id, text.len());
            }
        } else {
            self.cursor = BlockCursor::default();
        }

        // Clear pending ops again (the insert generated one)
        let _ = self.doc.take_pending_ops();

        // Not dirty - server is authoritative
        self.dirty = false;
    }

    /// Clear all content.
    pub fn clear(&mut self) {
        let block_ids: Vec<_> = self
            .doc
            .blocks_ordered()
            .iter()
            .map(|b| b.id.clone())
            .collect();
        for id in block_ids {
            let _ = self.doc.delete_block(&id);
        }
        self.cursor = BlockCursor::default();
        self.dirty = true;
    }

    /// Insert text at cursor position.
    pub fn insert(&mut self, text: &str) {
        // Ensure we have a block to insert into
        if self.cursor.block_id.is_none() {
            // Create a new text block
            if let Ok(block_id) = self.doc.insert_text_block(None, "") {
                self.cursor.block_id = Some(block_id);
                self.cursor.offset = 0;
            } else {
                return;
            }
        }

        if let Some(ref block_id) = self.cursor.block_id {
            if self
                .doc
                .edit_text(block_id, self.cursor.offset, text, 0)
                .is_ok()
            {
                self.cursor.offset += text.len();
                self.dirty = true;
            }
        }
    }

    /// Delete character before cursor (backspace).
    pub fn backspace(&mut self) {
        if self.cursor.offset == 0 {
            return; // At start, nothing to delete
        }

        if let Some(ref block_id) = self.cursor.block_id {
            // Find previous character boundary
            if let Some(block) = self.doc.get_block(block_id) {
                let text = block.text();
                let mut new_offset = self.cursor.offset.saturating_sub(1);
                while new_offset > 0 && !text.is_char_boundary(new_offset) {
                    new_offset -= 1;
                }
                let delete_len = self.cursor.offset - new_offset;

                if self
                    .doc
                    .edit_text(block_id, new_offset, "", delete_len)
                    .is_ok()
                {
                    self.cursor.offset = new_offset;
                    self.dirty = true;
                }
            }
        }
    }

    /// Delete character at cursor (delete key).
    pub fn delete(&mut self) {
        if let Some(ref block_id) = self.cursor.block_id {
            if let Some(block) = self.doc.get_block(block_id) {
                let text = block.text();
                if self.cursor.offset >= text.len() {
                    return; // At end, nothing to delete
                }

                // Find next character boundary
                let mut end = self.cursor.offset + 1;
                while end < text.len() && !text.is_char_boundary(end) {
                    end += 1;
                }
                let delete_len = end - self.cursor.offset;

                if self
                    .doc
                    .edit_text(block_id, self.cursor.offset, "", delete_len)
                    .is_ok()
                {
                    self.dirty = true;
                }
            }
        }
    }

    // =========================================================================
    // CURSOR MOVEMENT
    // =========================================================================

    /// Move cursor left.
    pub fn move_left(&mut self) {
        if self.cursor.offset > 0 {
            if let Some(ref block_id) = self.cursor.block_id {
                if let Some(block) = self.doc.get_block(block_id) {
                    let text = block.text();
                    let mut new_offset = self.cursor.offset - 1;
                    while new_offset > 0 && !text.is_char_boundary(new_offset) {
                        new_offset -= 1;
                    }
                    self.cursor.offset = new_offset;
                }
            }
        }
    }

    /// Move cursor right.
    pub fn move_right(&mut self) {
        if let Some(ref block_id) = self.cursor.block_id {
            if let Some(block) = self.doc.get_block(block_id) {
                let text = block.text();
                if self.cursor.offset < text.len() {
                    let mut new_offset = self.cursor.offset + 1;
                    while new_offset < text.len() && !text.is_char_boundary(new_offset) {
                        new_offset += 1;
                    }
                    self.cursor.offset = new_offset;
                }
            }
        }
    }

    /// Move cursor to start of current block.
    pub fn move_home(&mut self) {
        if let Some(ref block_id) = self.cursor.block_id {
            if let Some(block) = self.doc.get_block(block_id) {
                let text = block.text();
                // Find previous newline or start
                let before_cursor = &text[..self.cursor.offset];
                self.cursor.offset = before_cursor.rfind('\n').map(|i| i + 1).unwrap_or(0);
            }
        }
    }

    /// Move cursor to end of current line.
    pub fn move_end(&mut self) {
        if let Some(ref block_id) = self.cursor.block_id {
            if let Some(block) = self.doc.get_block(block_id) {
                let text = block.text();
                let after_cursor = &text[self.cursor.offset..];
                self.cursor.offset = self.cursor.offset
                    + after_cursor.find('\n').unwrap_or(after_cursor.len());
            }
        }
    }

    // =========================================================================
    // BLOCK OPERATIONS
    // =========================================================================

    /// Toggle collapse state of a thinking block.
    pub fn toggle_block_collapse(&mut self, block_id: &BlockId) {
        if let Some(block) = self.doc.get_block(block_id) {
            let new_state = !block.content.is_collapsed();
            let _ = self.doc.set_collapsed(block_id, new_state);
            self.dirty = true;
        }
    }

    // =========================================================================
    // SYNC OPERATIONS
    // =========================================================================

    /// Take pending operations for sending to server.
    pub fn take_pending_ops(&mut self) -> Vec<BlockDocOp> {
        self.doc.take_pending_ops()
    }

    /// Mark as synced.
    pub fn mark_synced(&mut self) {
        self.dirty = false;
        // Clear any remaining pending ops
        let _ = self.doc.take_pending_ops();
    }

    /// Apply a remote block insertion.
    pub fn apply_remote_block_insert(
        &mut self,
        block_id: BlockId,
        after_id: Option<BlockId>,
        content: BlockContentSnapshot,
    ) -> Result<(), String> {
        // Create the operation and apply it
        let op = BlockDocOp::InsertBlock {
            id: block_id,
            after: after_id,
            content,
            author: "remote".to_string(), // Remote insertions
            created_at: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0),
            fugue_meta: None,
        };
        self.doc.apply_remote_op(&op).map_err(|e| e.to_string())
    }

    /// Apply a remote block deletion.
    pub fn apply_remote_block_delete(&mut self, block_id: &BlockId) -> Result<(), String> {
        let op = BlockDocOp::DeleteBlock { id: block_id.clone() };
        self.doc.apply_remote_op(&op).map_err(|e| e.to_string())
    }

    /// Apply a remote text edit.
    pub fn apply_remote_block_edit(
        &mut self,
        block_id: &BlockId,
        pos: usize,
        insert: &str,
        delete: usize,
    ) -> Result<(), String> {
        let op = BlockDocOp::EditBlockText {
            id: block_id.clone(),
            pos,
            insert: insert.to_string(),
            delete,
            dt_encoded: None,
        };
        self.doc.apply_remote_op(&op).map_err(|e| e.to_string())
    }

    /// Apply a remote collapsed state change.
    pub fn apply_remote_block_collapsed(
        &mut self,
        block_id: &BlockId,
        collapsed: bool,
    ) -> Result<(), String> {
        let op = BlockDocOp::SetCollapsed {
            id: block_id.clone(),
            collapsed,
        };
        self.doc.apply_remote_op(&op).map_err(|e| e.to_string())
    }

    /// Apply a remote block move.
    pub fn apply_remote_block_move(
        &mut self,
        block_id: &BlockId,
        after_id: Option<&BlockId>,
    ) -> Result<(), String> {
        let op = BlockDocOp::MoveBlock {
            id: block_id.clone(),
            after: after_id.cloned(),
            fugue_meta: None,
        };
        self.doc.apply_remote_op(&op).map_err(|e| e.to_string())
    }

    /// Apply server-authoritative block state (full replacement).
    pub fn apply_server_block_state(
        &mut self,
        blocks: Vec<(BlockId, BlockContentSnapshot)>,
        _version: u64,
    ) {
        // Clear existing blocks
        let existing_ids: Vec<_> = self.doc.blocks_ordered().iter().map(|b| b.id.clone()).collect();
        for id in existing_ids {
            let _ = self.doc.delete_block(&id);
        }

        // Insert blocks from server
        let mut prev_id: Option<BlockId> = None;
        for (block_id, content) in blocks {
            let op = BlockDocOp::InsertBlock {
                id: block_id.clone(),
                after: prev_id.clone(),
                content,
                author: "server".to_string(),
                created_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0),
                fugue_meta: None,
            };
            let _ = self.doc.apply_remote_op(&op);
            prev_id = Some(block_id);
        }

        // Clear pending ops - server state is authoritative
        let _ = self.doc.take_pending_ops();

        // Update cursor
        if let Some(first_block) = self.doc.blocks_ordered().first() {
            self.cursor = BlockCursor::at(first_block.id.clone(), 0);
        } else {
            self.cursor = BlockCursor::default();
        }
    }
}

// ============================================================================
// LAYOUT AND STATE COMPONENTS
// ============================================================================

/// Position of a cell in the workspace grid.
#[derive(Component, Default, Clone, Copy)]
pub struct CellPosition {
    /// Row (0-indexed)
    pub row: u32,
}

impl CellPosition {
    pub fn new(row: u32) -> Self {
        Self { row }
    }
}

/// Visual state of a cell.
#[derive(Component, Default, Clone)]
pub struct CellState {
    /// Whether this cell is collapsed (children hidden)
    pub collapsed: bool,
    /// Computed height based on content (in pixels)
    pub computed_height: f32,
}

impl CellState {
    pub fn new() -> Self {
        Self {
            collapsed: false,
            computed_height: 100.0,
        }
    }
}

/// Vim-style editor mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EditorMode {
    /// Navigation mode (h/j/k/l, commands)
    #[default]
    Normal,
    /// Text input mode
    Insert,
    /// Command-line mode (: prefix)
    Command,
    /// Visual selection mode
    Visual,
}

impl EditorMode {
    pub fn name(&self) -> &'static str {
        match self {
            EditorMode::Normal => "NORMAL",
            EditorMode::Insert => "INSERT",
            EditorMode::Command => "COMMAND",
            EditorMode::Visual => "VISUAL",
        }
    }
}

/// Resource tracking the current editor mode.
#[derive(Resource, Default)]
pub struct CurrentMode(pub EditorMode);

/// Resource tracking which cell has keyboard focus.
#[derive(Resource, Default)]
pub struct FocusedCell(pub Option<Entity>);

/// Configuration for workspace layout.
#[derive(Resource)]
pub struct WorkspaceLayout {
    /// Minimum cell height
    pub min_cell_height: f32,
    /// Maximum cell height (0 = unlimited)
    pub max_cell_height: f32,
    /// Left margin for the workspace
    pub workspace_margin_left: f32,
    /// Top margin for the workspace
    pub workspace_margin_top: f32,
    /// Line height for computing dynamic heights
    pub line_height: f32,
    /// Total height reserved for prompt area at bottom (prompt + status bar + padding)
    pub prompt_area_height: f32,
    /// Minimum height for prompt cell
    pub prompt_min_height: f32,
    /// Distance from bottom of window to prompt cell top
    pub prompt_bottom_offset: f32,
}

impl Default for WorkspaceLayout {
    fn default() -> Self {
        Self {
            min_cell_height: 60.0,
            max_cell_height: 400.0,
            workspace_margin_left: 20.0,
            workspace_margin_top: 70.0, // Space for compact header
            line_height: 20.0,
            prompt_area_height: 120.0,  // Space reserved at bottom for prompt
            prompt_min_height: 50.0,    // Minimum prompt cell height
            prompt_bottom_offset: 100.0, // Prompt Y = window_height - this
        }
    }
}

impl WorkspaceLayout {
    /// Calculate dynamic cell height based on line count.
    pub fn height_for_lines(&self, line_count: usize) -> f32 {
        let content_height = (line_count as f32) * self.line_height + 24.0; // padding
        let height = content_height.max(self.min_cell_height);
        if self.max_cell_height > 0.0 {
            height.min(self.max_cell_height)
        } else {
            height
        }
    }
}

// ============================================================================
// CONVERSATION UI LAYOUT COMPONENTS
// ============================================================================

/// Marker for the scrollable conversation container.
/// Holds message cells (UserMessage, AgentMessage, tool calls).
#[derive(Component)]
pub struct ConversationContainer;

/// Marker for the fixed prompt input area at the bottom.
#[derive(Component)]
pub struct PromptContainer;

/// Marker for the prompt input cell (the editable text input at bottom).
/// This cell captures input in INSERT mode and submits on Enter.
#[derive(Component)]
pub struct PromptCell;

/// Marker for the main kernel/shell cell.
/// This is the primary workspace cell that displays kernel output and shell interactions.
#[derive(Component)]
pub struct MainCell;

/// Message fired when user submits prompt text (presses Enter).
#[derive(Message)]
pub struct PromptSubmitted {
    /// The text that was submitted.
    pub text: String,
}

/// Resource tracking the conversation scroll position.
#[derive(Resource)]
pub struct ConversationScrollState {
    /// Current scroll offset (pixels from top, 0 = at top)
    pub offset: f32,
    /// Total content height (computed from all cells)
    pub content_height: f32,
    /// Visible height of the conversation area
    pub visible_height: f32,
    /// Whether we should auto-scroll to bottom on next frame.
    pub scroll_to_bottom: bool,
}

impl Default for ConversationScrollState {
    fn default() -> Self {
        Self {
            offset: 0.0,
            content_height: 0.0,
            visible_height: 600.0, // Will be updated by layout system
            scroll_to_bottom: false,
        }
    }
}

impl ConversationScrollState {
    /// Maximum scroll offset (can't scroll past content)
    pub fn max_offset(&self) -> f32 {
        (self.content_height - self.visible_height).max(0.0)
    }

    /// Clamp the current offset to valid bounds
    pub fn clamp_offset(&mut self) {
        self.offset = self.offset.clamp(0.0, self.max_offset());
    }

    /// Scroll by a delta amount (positive = scroll down)
    pub fn scroll_by(&mut self, delta: f32) {
        self.offset += delta;
        self.clamp_offset();
    }

    /// Scroll to the bottom of the content
    pub fn scroll_to_end(&mut self) {
        self.offset = self.max_offset();
    }
}
