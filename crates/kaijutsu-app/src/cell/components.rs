//! Cell components for Bevy ECS.
//!
//! Cells are the fundamental content primitive in Kaijutsu. Each cell contains
//! structured content blocks (text, thinking, tool use/results) managed by CRDTs.

use bevy::prelude::*;

// Re-export CRDT types for convenience
// NOTE: BlockContentSnapshot was replaced with flat BlockSnapshot in the DAG migration
pub use kaijutsu_crdt::{BlockDocument, BlockId, BlockKind, BlockSnapshot, Role};

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
///
/// Note: Some variants correspond to server-side cell types that aren't
/// yet rendered in the client UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[allow(dead_code)] // Variants map to server types
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
}

impl Cell {
    pub fn new() -> Self {
        Self { id: CellId::new() }
    }
}

impl Default for Cell {
    fn default() -> Self {
        Self::new()
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
        if !text.is_empty()
            && let Ok(block_id) = self.doc.insert_block(None, None, Role::User, BlockKind::Text, &text, "user") {
                self.cursor = BlockCursor::at(block_id, text.len());
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
    pub fn blocks(&self) -> Vec<BlockSnapshot> {
        self.doc.blocks_ordered()
    }

    // =========================================================================
    // TEXT MUTATION
    // =========================================================================

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
            if let Ok(block_id) = self.doc.insert_block(None, None, Role::User, BlockKind::Text, "", "user") {
                self.cursor.block_id = Some(block_id);
                self.cursor.offset = 0;
            } else {
                return;
            }
        }

        if let Some(ref block_id) = self.cursor.block_id
            && self
                .doc
                .edit_text(block_id, self.cursor.offset, text, 0)
                .is_ok()
            {
                self.cursor.offset += text.len();
                self.dirty = true;
            }
    }

    /// Delete character before cursor (backspace).
    pub fn backspace(&mut self) {
        if self.cursor.offset == 0 {
            return; // At start, nothing to delete
        }

        if let Some(ref block_id) = self.cursor.block_id {
            // Find previous character boundary
            if let Some(block) = self.doc.get_block_snapshot(block_id) {
                let text = block.content.clone();
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
        if let Some(ref block_id) = self.cursor.block_id
            && let Some(block) = self.doc.get_block_snapshot(block_id) {
                let text = block.content.clone();
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

    // =========================================================================
    // CURSOR MOVEMENT
    // =========================================================================

    /// Move cursor left.
    pub fn move_left(&mut self) {
        if self.cursor.offset > 0
            && let Some(ref block_id) = self.cursor.block_id
                && let Some(block) = self.doc.get_block_snapshot(block_id) {
                    let text = block.content.clone();
                    let mut new_offset = self.cursor.offset - 1;
                    while new_offset > 0 && !text.is_char_boundary(new_offset) {
                        new_offset -= 1;
                    }
                    self.cursor.offset = new_offset;
                }
    }

    /// Move cursor right.
    pub fn move_right(&mut self) {
        if let Some(ref block_id) = self.cursor.block_id
            && let Some(block) = self.doc.get_block_snapshot(block_id) {
                let text = block.content.clone();
                if self.cursor.offset < text.len() {
                    let mut new_offset = self.cursor.offset + 1;
                    while new_offset < text.len() && !text.is_char_boundary(new_offset) {
                        new_offset += 1;
                    }
                    self.cursor.offset = new_offset;
                }
            }
    }

    /// Move cursor to start of current block.
    pub fn move_home(&mut self) {
        if let Some(ref block_id) = self.cursor.block_id
            && let Some(block) = self.doc.get_block_snapshot(block_id) {
                let text = block.content.clone();
                // Find previous newline or start
                let before_cursor = &text[..self.cursor.offset];
                self.cursor.offset = before_cursor.rfind('\n').map(|i| i + 1).unwrap_or(0);
            }
    }

    /// Move cursor to end of current line.
    pub fn move_end(&mut self) {
        if let Some(ref block_id) = self.cursor.block_id
            && let Some(block) = self.doc.get_block_snapshot(block_id) {
                let text = block.content.clone();
                let after_cursor = &text[self.cursor.offset..];
                self.cursor.offset += after_cursor.find('\n').unwrap_or(after_cursor.len());
            }
    }

    // =========================================================================
    // BLOCK OPERATIONS
    // =========================================================================

    /// Toggle collapse state of a thinking block.
    pub fn toggle_block_collapse(&mut self, block_id: &BlockId) {
        if let Some(block) = self.doc.get_block_snapshot(block_id) {
            let new_state = !block.collapsed;
            let _ = self.doc.set_collapsed(block_id, new_state);
            self.dirty = true;
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
    /// Minimum height for prompt cell
    pub prompt_min_height: f32,
}

impl Default for WorkspaceLayout {
    fn default() -> Self {
        Self {
            min_cell_height: 60.0,
            max_cell_height: 400.0,
            workspace_margin_left: 20.0,
            workspace_margin_top: 70.0, // Space for compact header
            line_height: 20.0,
            prompt_min_height: 50.0,
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
///
/// FUTURE: The prompt might become mobile - attaching to focused BlockCells
/// for threaded reply workflows instead of being fixed at bottom.
#[derive(Component)]
pub struct PromptContainer;

/// Marker for the prompt input cell (the editable text input at bottom).
/// This cell captures input in INSERT mode and submits on Enter.
///
/// FUTURE: Input capabilities might move to BlockCells directly, or
/// PromptCell could "attach" to a focused block for reply-to workflows.
/// Current implementation uses legacy GlyphonText + TextAreaConfig rendering.
#[derive(Component)]
pub struct PromptCell;

/// Marker for the main conversation view cell.
///
/// NOTE: MainCell no longer renders directly - it holds the CellEditor
/// (source of truth for content) while BlockCells handle per-block rendering.
/// Kept as the "owner" entity for BlockCellContainer and TurnCellContainer.
#[derive(Component)]
pub struct MainCell;

/// Message fired when user submits prompt text (presses Enter).
#[derive(Message)]
pub struct PromptSubmitted {
    /// The text that was submitted.
    pub text: String,
}

/// Resource tracking the conversation scroll position.
///
/// Implements terminal-style smooth scrolling:
/// - `offset` is the current rendered position
/// - `target_offset` is where we're scrolling toward
/// - `following` enables auto-tracking bottom during streaming
#[derive(Resource)]
pub struct ConversationScrollState {
    /// Current scroll offset (pixels from top, 0 = at top).
    /// This is the rendered position, interpolated toward target_offset.
    pub offset: f32,
    /// Target scroll offset we're smoothly scrolling toward.
    pub target_offset: f32,
    /// Total content height (computed from all cells)
    pub content_height: f32,
    /// Visible height of the conversation area
    pub visible_height: f32,
    /// Follow mode: continuously track bottom during streaming.
    /// When true, target_offset auto-updates to max_offset each frame.
    /// Set to false when user manually scrolls up.
    pub following: bool,
}

impl Default for ConversationScrollState {
    fn default() -> Self {
        Self {
            offset: 0.0,
            target_offset: 0.0,
            content_height: 0.0,
            visible_height: 600.0, // Will be updated by layout system
            following: true, // Start in follow mode
        }
    }
}

impl ConversationScrollState {
    /// Check if scroll position is at (or near) the bottom.
    /// Used to determine if we should enter/stay in follow mode.
    pub fn is_at_bottom(&self) -> bool {
        // Within 50px of max scroll counts as "at bottom"
        const THRESHOLD: f32 = 50.0;
        self.target_offset >= self.max_offset() - THRESHOLD
    }

    /// Maximum scroll offset (can't scroll past content)
    pub fn max_offset(&self) -> f32 {
        (self.content_height - self.visible_height).max(0.0)
    }

    /// Clamp a value to valid scroll bounds
    fn clamp_to_bounds(&self, value: f32) -> f32 {
        value.clamp(0.0, self.max_offset())
    }

    /// Clamp target offset to valid bounds
    pub fn clamp_target(&mut self) {
        self.target_offset = self.clamp_to_bounds(self.target_offset);
    }

    /// Scroll by a delta amount (positive = scroll down).
    /// Instant - sets both offset and target for zero-frame-delay.
    pub fn scroll_by(&mut self, delta: f32) {
        // If scrolling up, disable follow mode
        if delta < 0.0 {
            self.following = false;
        }

        // Set both for instant response
        self.target_offset += delta;
        self.clamp_target();
        self.offset = self.target_offset;

        // If scrolling down and we hit bottom, re-enable follow mode
        if self.is_at_bottom() {
            self.following = true;
        }
    }

    /// Set target to bottom and enable follow mode.
    pub fn scroll_to_end(&mut self) {
        self.target_offset = self.max_offset();
        self.following = true;
    }

    /// Enable follow mode (will smoothly scroll to and track bottom).
    pub fn start_following(&mut self) {
        self.following = true;
    }
}

// ============================================================================
// BLOCK-ORIENTED UI COMPONENTS
// ============================================================================
//
// ARCHITECTURE: Each conversation block becomes its own Bevy entity.
// This enables per-block streaming, independent collapse/expand, and
// future features like threaded replies.
//
// FUTURE DIRECTION:
// - BlockCells may become focusable for "reply to this block" workflows
// - Input area could attach to or follow the focused BlockCell
// - Consider: BlockCell gaining PromptCell-like input capabilities
// - Turn headers (TurnCell) group blocks by author for visual clarity
//
// Current state: BlockCells render read-only content. PromptCell handles input.

/// Marker for a UI entity representing a single content block.
///
/// Each block in a conversation gets its own entity with independent:
/// - GlyphonTextBuffer for rendering
/// - Layout positioning
/// - Change tracking (for efficient streaming updates)
///
/// FUTURE: May gain focus/input capabilities for threaded conversations.
#[derive(Component, Debug)]
pub struct BlockCell {
    /// The block ID this cell represents.
    pub block_id: BlockId,
    /// Last known content hash/version for dirty tracking.
    pub last_render_version: u64,
}

impl BlockCell {
    pub fn new(block_id: BlockId) -> Self {
        Self {
            block_id,
            last_render_version: 0,
        }
    }
}

/// Container that tracks all BlockCell entities for a conversation view.
///
/// Attached to the entity that owns the conversation display (e.g., MainCell parent).
#[derive(Component, Debug, Default)]
pub struct BlockCellContainer {
    /// Ordered list of BlockCell entities.
    pub block_cells: Vec<Entity>,
    /// Map from block ID to entity for fast lookup.
    pub block_to_entity: std::collections::HashMap<BlockId, Entity>,
}

impl BlockCellContainer {
    /// Add a new block cell.
    pub fn add(&mut self, block_id: BlockId, entity: Entity) {
        self.block_cells.push(entity);
        self.block_to_entity.insert(block_id, entity);
    }

    /// Remove a block cell by entity.
    pub fn remove(&mut self, entity: Entity) {
        self.block_cells.retain(|e| *e != entity);
        self.block_to_entity.retain(|_, e| *e != entity);
    }

    /// Get entity for a block ID.
    pub fn get_entity(&self, block_id: &BlockId) -> Option<Entity> {
        self.block_to_entity.get(block_id).copied()
    }

    /// Check if a block ID is already tracked.
    pub fn contains(&self, block_id: &BlockId) -> bool {
        self.block_to_entity.contains_key(block_id)
    }
}

/// Computed layout for a block cell.
#[derive(Component, Debug, Default)]
pub struct BlockCellLayout {
    /// Y position (top) relative to conversation content start.
    pub y_offset: f32,
    /// Computed height based on content.
    pub height: f32,
    /// Indentation level (for nested tool results).
    pub indent_level: u32,
}

// ============================================================================
// TURN UI COMPONENTS (Removed)
// ============================================================================
//
// Turn headers are now rendered inline based on role transitions.
// The layout_block_cells system handles role transition detection and
// reserves space for inline role headers. See systems.rs for details.
