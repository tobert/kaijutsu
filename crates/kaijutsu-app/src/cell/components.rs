//! Cell components for Bevy ECS.
//!
//! Cells are the fundamental content primitive in Kaijutsu. Each cell contains
//! structured content blocks (text, thinking, tool use/results) managed by CRDTs.

use bevy::prelude::*;

// Re-export CRDT types for convenience
// NOTE: BlockContentSnapshot was replaced with flat BlockSnapshot in the DAG migration
pub use kaijutsu_crdt::{BlockDocument, BlockId, BlockKind, BlockSnapshot, Role};

/// Unique identifier for a cell.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Reflect)]
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
#[derive(Component, Debug, Clone, Reflect)]
#[reflect(Component)]
pub struct ViewingConversation {
    /// ID of the conversation this cell is viewing.
    pub conversation_id: String,
    /// Last sync version to detect changes.
    pub last_sync_version: u64,
}

/// Core cell component - the fundamental content primitive.
#[derive(Component, Reflect)]
#[reflect(Component)]
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
#[derive(Debug, Clone, Default, Reflect)]
pub struct BlockCursor {
    /// Which block the cursor is in.
    #[reflect(ignore)]
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

/// Cached cursor screen position (row, col).
///
/// This avoids O(N) string scans every frame by caching the computed
/// position until the document version changes.
#[derive(Clone, Copy, Debug, Default, Reflect)]
pub struct CursorCache {
    /// Cached row (0-indexed)
    pub row: usize,
    /// Cached column (0-indexed)
    pub col: usize,
    /// Document version when cache was computed
    pub version: u64,
}

/// Text editor state for a cell.
///
/// The `doc` field (BlockDocument) is the single source of truth for all content.
/// All modifications go through the BlockDocument's CRDT operations.
///
/// Note: Not reflectable due to BlockDocument lacking Default.
/// Use query filters to find CellEditor entities instead of BRP inspection.
#[derive(Component)]
pub struct CellEditor {
    /// Block document - the source of truth for all content.
    pub doc: BlockDocument,

    /// Cursor position within the document.
    pub cursor: BlockCursor,

    /// Cached screen position for cursor rendering.
    pub cursor_cache: CursorCache,
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
            cursor_cache: CursorCache::default(),
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

                let _ = self
                    .doc
                    .edit_text(block_id, self.cursor.offset, "", delete_len);
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
        }
    }

}

// ============================================================================
// LAYOUT AND STATE COMPONENTS
// ============================================================================

/// Position of a cell in the workspace grid.
#[derive(Component, Default, Clone, Copy, Reflect)]
#[reflect(Component)]
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
#[derive(Component, Default, Clone, Reflect)]
#[reflect(Component)]
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

/// Input routing kind for text entry modes.
///
/// Determines where submitted text goes:
/// - **Chat**: Routes to LLM for AI conversation
/// - **Shell**: Routes to kaish REPL (handles both shell commands and `:` commands)
#[derive(Debug, Clone, Copy, PartialEq, Eq, Reflect)]
pub enum InputKind {
    /// Prompts go to LLM
    Chat,
    /// Prompts go to kaish REPL
    Shell,
}

impl InputKind {
    pub fn name(&self) -> &'static str {
        match self {
            InputKind::Chat => "CHAT",
            InputKind::Shell => "SHELL",
        }
    }
}

/// Vim-style editor mode (simplified from 5 to 3 modes).
///
/// The old Command mode is folded into Shell - kaish handles `:` commands natively.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
pub enum EditorMode {
    /// Navigation mode (h/j/k/l, block focus)
    #[default]
    Normal,
    /// Text input mode with routing to Chat or Shell
    Input(InputKind),
    /// Visual selection mode
    Visual,
}

impl EditorMode {
    pub fn name(&self) -> &'static str {
        match self {
            EditorMode::Normal => "NORMAL",
            EditorMode::Input(kind) => kind.name(),
            EditorMode::Visual => "VISUAL",
        }
    }

    /// Check if this mode accepts text input.
    pub fn accepts_input(&self) -> bool {
        matches!(self, EditorMode::Input(_))
    }

    /// Get the input kind if in Input mode.
    #[allow(dead_code)]
    pub fn input_kind(&self) -> Option<InputKind> {
        match self {
            EditorMode::Input(kind) => Some(*kind),
            _ => None,
        }
    }

    /// Check if this is Chat input mode.
    #[allow(dead_code)]
    pub fn is_chat(&self) -> bool {
        matches!(self, EditorMode::Input(InputKind::Chat))
    }

    /// Check if this is Shell input mode.
    #[allow(dead_code)]
    pub fn is_shell(&self) -> bool {
        matches!(self, EditorMode::Input(InputKind::Shell))
    }
}

/// Resource tracking the current editor mode.
#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct CurrentMode(pub EditorMode);

/// Resource tracking which cell has keyboard focus.
#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct FocusedCell(pub Option<Entity>);

// ============================================================================
// BLOCK FOCUS NAVIGATION (Phase 2)
// ============================================================================

/// Which block is focused for navigation/reply.
///
/// This enables j/k navigation between blocks (not just scroll) and
/// supports future "reply to this block" workflows.
#[derive(Resource, Default)]
pub struct ConversationFocus {
    /// The currently focused block (if any).
    pub block_id: Option<BlockId>,
}

impl ConversationFocus {
    /// Check if a specific block is focused.
    #[allow(dead_code)]
    pub fn is_focused(&self, block_id: &BlockId) -> bool {
        self.block_id.as_ref() == Some(block_id)
    }

    /// Clear the focus.
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.block_id = None;
    }

    /// Set focus to a block.
    pub fn focus(&mut self, block_id: BlockId) {
        self.block_id = Some(block_id);
    }
}

/// Marker for the currently focused block cell.
///
/// Added/removed by the navigate_blocks system to enable visual feedback
/// and future reply-target workflows.
#[derive(Component)]
pub struct FocusedBlockCell;

/// Marker for a block cell that is currently being edited.
///
/// When this marker is present on a BlockCell entity, the block receives
/// keyboard input and the cursor is displayed within it. This is the core
/// of the "any block can be edited" model.
///
/// Added when: User presses `i` with a FocusedBlockCell active
/// Removed when: User presses `Escape` to exit edit mode
#[derive(Component)]
pub struct EditingBlockCell;

/// Tracks the edit cursor position within an editing block.
///
/// This is separate from CellEditor's cursor because BlockCells don't have
/// a full CellEditor - they render from the MainCell's BlockDocument.
/// The cursor is an offset within the block's content string.
#[derive(Component, Default)]
pub struct BlockEditCursor {
    /// Character offset within the block's content.
    pub offset: usize,
}

/// Resource tracking CRDT sync state for frontier-based incremental updates.
///
/// This is a thin wrapper around [`SyncManager`](super::sync::SyncManager) that
/// integrates with Bevy ECS. The actual sync logic is in SyncManager, which can
/// be unit tested without Bevy dependencies.
///
/// **Sync protocol:**
/// - `frontier = None` or `cell_id` changed → full sync (from_oplog)
/// - `frontier = Some(_)` and matching cell_id → incremental merge (merge_ops_owned)
#[derive(Resource, Default)]
pub struct DocumentSyncState(pub super::sync::SyncManager);

impl std::ops::Deref for DocumentSyncState {
    type Target = super::sync::SyncManager;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for DocumentSyncState {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

/// Configuration for workspace layout.
#[derive(Resource, Reflect)]
#[reflect(Resource)]
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
            line_height: 22.5, // Match TextMetrics.cell_line_height for cursor alignment
            prompt_min_height: 50.0,
        }
    }
}

impl WorkspaceLayout {
    /// Calculate dynamic cell height based on line count.
    pub fn height_for_lines(&self, line_count: usize) -> f32 {
        let content_height = (line_count as f32) * self.line_height + 4.0; // tight padding
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
#[derive(Component, Reflect)]
#[reflect(Component)]
pub struct ConversationContainer;

/// Marker for the fixed prompt input area at the bottom.
///
/// FUTURE: The prompt might become mobile - attaching to focused BlockCells
/// for threaded reply workflows instead of being fixed at bottom.
#[derive(Component, Reflect)]
#[reflect(Component)]
pub struct PromptContainer;

/// Marker for the prompt input cell (the editable text input at bottom).
/// This cell captures input in INSERT mode and submits on Enter.
///
/// FUTURE: Input capabilities might move to BlockCells directly, or
/// PromptCell could "attach" to a focused block for reply-to workflows.
/// Current implementation uses legacy GlyphonText + TextAreaConfig rendering.
#[derive(Component, Reflect)]
#[reflect(Component)]
pub struct PromptCell;

/// Marker for the compose block - an inline editable block at the end of conversation.
///
/// The compose block is the "unified edit model" replacement for the floating prompt:
/// - Renders inline with conversation blocks (scrolls with content)
/// - Always editable (no need to enter edit mode)
/// - Styled like a user block but with distinct border
/// - Submitting creates a new block and clears the compose area
///
/// This makes the input area part of the conversation flow rather than
/// a separate floating element.
#[derive(Component, Reflect, Default)]
#[reflect(Component)]
pub struct ComposeBlock {
    /// Current text content (before submission)
    pub text: String,
    /// Cursor position within the text
    pub cursor: usize,
}

impl ComposeBlock {
    /// Insert text at cursor position
    pub fn insert(&mut self, s: &str) {
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Delete character before cursor (backspace)
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            // Find the previous char boundary
            let prev = self.text[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.text.drain(prev..self.cursor);
            self.cursor = prev;
        }
    }

    /// Delete character after cursor (delete)
    pub fn delete(&mut self) {
        if self.cursor < self.text.len() {
            // Find the next char boundary
            let next = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
            self.text.drain(self.cursor..next);
        }
    }

    /// Move cursor left
    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor = self.text[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    /// Move cursor right
    pub fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
        }
    }

    /// Clear and return the text (for submission)
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        std::mem::take(&mut self.text)
    }

    /// Check if the compose block is empty
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

/// Marker for the main conversation view cell.
///
/// NOTE: MainCell no longer renders directly - it holds the CellEditor
/// (source of truth for content) while BlockCells handle per-block rendering.
/// Kept as the "owner" entity for BlockCellContainer and TurnCellContainer.
#[derive(Component, Reflect)]
#[reflect(Component)]
pub struct MainCell;

/// Message fired when user submits prompt text (presses Enter).
#[derive(Message, Reflect)]
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
#[derive(Resource, Reflect)]
#[reflect(Resource)]
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
    /// Set to true when user explicitly scrolls this frame.
    /// Prevents handle_block_events from re-enabling following mode.
    /// Cleared each frame by smooth_scroll.
    #[reflect(ignore)]
    pub user_scrolled_this_frame: bool,
    /// Last LayoutGeneration value when we checked for auto-scroll.
    /// Used to detect content changes for scroll auto-follow.
    #[reflect(ignore)]
    pub last_content_gen: u64,
}

impl Default for ConversationScrollState {
    fn default() -> Self {
        Self {
            offset: 0.0,
            target_offset: 0.0,
            content_height: 0.0,
            visible_height: 600.0, // Will be updated by layout system
            following: true, // Start in follow mode
            user_scrolled_this_frame: false,
            last_content_gen: 0,
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
        // Mark that user explicitly scrolled this frame
        // This prevents handle_block_events from re-enabling following
        self.user_scrolled_this_frame = true;

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
// LAYOUT GENERATION TRACKING
// ============================================================================

/// Tracks when block layout needs recomputation.
///
/// Incremented by systems that modify block content. Layout systems
/// compare against their last-seen generation to skip redundant work.
/// This is the key optimization for scroll performance: when scrolling,
/// content hasn't changed, so we skip the expensive layout recomputation.
#[derive(Resource, Default)]
pub struct LayoutGeneration(pub u64);

impl LayoutGeneration {
    /// Bump the generation counter, signaling that layout needs recomputation.
    pub fn bump(&mut self) {
        self.0 = self.0.wrapping_add(1);
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
///
/// Note: Not reflectable due to BlockId lacking Default.
#[derive(Component, Debug)]
pub struct BlockCell {
    /// The block ID this cell represents.
    pub block_id: BlockId,
    /// Last known content hash/version for dirty tracking.
    pub last_render_version: u64,
    /// Last known visual line count for layout dirty tracking.
    /// Only bump LayoutGeneration when this changes.
    pub last_line_count: usize,
}

impl BlockCell {
    pub fn new(block_id: BlockId) -> Self {
        Self {
            block_id,
            last_render_version: 0,
            last_line_count: 0,
        }
    }
}

/// Container that tracks all BlockCell entities for a conversation view.
///
/// Attached to the entity that owns the conversation display (e.g., MainCell parent).
#[derive(Component, Debug, Default, Reflect)]
#[reflect(Component)]
pub struct BlockCellContainer {
    /// Ordered list of BlockCell entities.
    pub block_cells: Vec<Entity>,
    /// Map from block ID to entity for fast lookup.
    #[reflect(ignore)]
    pub block_to_entity: std::collections::HashMap<BlockId, Entity>,
    /// Role header entities (one per role transition).
    pub role_headers: Vec<Entity>,
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
#[derive(Component, Debug, Default, Reflect)]
#[reflect(Component)]
pub struct BlockCellLayout {
    /// Y position (top) relative to conversation content start.
    pub y_offset: f32,
    /// Computed height based on content.
    pub height: f32,
    /// Indentation level (for nested tool results).
    pub indent_level: u32,
}

// ============================================================================
// ROLE HEADER COMPONENTS
// ============================================================================

/// Role header entity that appears before first block of each turn.
/// Rendered as a styled, distinct header separate from block content.
///
/// Note: Not fully reflectable due to BlockId lacking Default.
#[derive(Component, Debug, Clone)]
pub struct RoleHeader {
    /// The role this header represents.
    pub role: kaijutsu_crdt::Role,
    /// The block ID this header precedes (for layout positioning).
    pub block_id: BlockId,
}

/// Layout information for a role header.
#[derive(Component, Debug, Default, Reflect)]
#[reflect(Component)]
pub struct RoleHeaderLayout {
    /// Y position (top) relative to conversation content start.
    pub y_offset: f32,
}
