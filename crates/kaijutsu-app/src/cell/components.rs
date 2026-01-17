//! Cell components for Bevy ECS.

use bevy::prelude::*;
use serde::{Deserialize, Serialize};

/// Unique identifier for a cell.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CellId(pub String);

impl CellId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn from_str(s: &str) -> Self {
        Self(s.to_string())
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

    pub fn markdown() -> Self {
        Self::new(CellKind::Markdown)
    }

    pub fn output() -> Self {
        Self::new(CellKind::Output)
    }

    pub fn with_parent(mut self, parent: CellId) -> Self {
        self.parent = Some(parent);
        self
    }

    pub fn with_id(mut self, id: CellId) -> Self {
        self.id = id;
        self
    }
}

/// A single edit operation for CRDT sync (text-level).
#[derive(Debug, Clone)]
pub enum EditOp {
    /// Insert text at position
    Insert { pos: usize, text: String },
    /// Delete characters at position
    Delete { pos: usize, len: usize },
}

// ============================================================================
// CONTENT BLOCK MODEL
// ============================================================================

/// A block of content within a cell.
///
/// This mirrors Claude's API response format to support structured content
/// display: extended thinking, tool use, text blocks, and tool results.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContentBlock {
    /// Model's extended thinking/reasoning (collapsible when complete).
    Thinking {
        /// The reasoning text.
        text: String,
        /// Whether the block is collapsed in the UI.
        #[serde(default)]
        collapsed: bool,
    },
    /// Main text response.
    Text(String),
    /// Tool invocation request.
    ToolUse {
        /// Unique ID for this tool use.
        id: String,
        /// Tool name.
        name: String,
        /// Tool input as JSON.
        input: serde_json::Value,
    },
    /// Result from a tool execution.
    ToolResult {
        /// ID of the tool_use this is a result for.
        tool_use_id: String,
        /// Result content.
        content: String,
        /// Whether this result represents an error.
        is_error: bool,
    },
}

impl ContentBlock {
    /// Get the text content of this block, if any.
    pub fn text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text(s) => Some(s),
            ContentBlock::Thinking { text, .. } => Some(text),
            ContentBlock::ToolResult { content, .. } => Some(content),
            ContentBlock::ToolUse { .. } => None,
        }
    }

    /// Check if this is a thinking block.
    pub fn is_thinking(&self) -> bool {
        matches!(self, ContentBlock::Thinking { .. })
    }

    /// Check if this is a tool use block.
    pub fn is_tool_use(&self) -> bool {
        matches!(self, ContentBlock::ToolUse { .. })
    }

    /// Check if this block is collapsed (only applies to thinking blocks).
    pub fn is_collapsed(&self) -> bool {
        match self {
            ContentBlock::Thinking { collapsed, .. } => *collapsed,
            _ => false,
        }
    }

    /// Create a text block.
    pub fn text_block(text: impl Into<String>) -> Self {
        ContentBlock::Text(text.into())
    }

    /// Create a thinking block (auto-collapses when complete).
    pub fn thinking(text: impl Into<String>) -> Self {
        ContentBlock::Thinking {
            text: text.into(),
            collapsed: true,
        }
    }
}

/// Operations on content blocks for CRDT sync.
#[derive(Debug, Clone)]
pub enum BlockOp {
    /// Insert a new block at index.
    InsertBlock { index: usize, block: ContentBlock },
    /// Delete block at index.
    DeleteBlock { index: usize },
    /// Edit text within a block (for Text/Thinking blocks).
    EditBlock { index: usize, op: EditOp },
    /// Toggle thinking block collapsed state.
    CollapseBlock { index: usize, collapsed: bool },
    /// Replace all blocks (full sync).
    ReplaceAll { blocks: Vec<ContentBlock> },
}

/// Text editor state for a cell.
///
/// All content modifications go through CRDT operations to ensure
/// proper synchronization across multiple clients.
///
/// Content is stored as structured blocks (thinking, text, tool_use, etc.).
/// The `text_cache` field is derived from blocks and should never be set directly.
#[derive(Component)]
pub struct CellEditor {
    /// Structured content blocks - the source of truth.
    pub blocks: Vec<ContentBlock>,
    /// Which block the cursor is in (0-indexed).
    pub cursor_block: usize,
    /// Cursor position within the current block (byte offset).
    pub cursor_offset: usize,
    /// Cached text representation (derived from blocks).
    /// Private - use `text()` accessor.
    text_cache: String,
    /// Cursor position in the cached text (byte offset).
    pub cursor: usize,
    /// Selection start (byte offset, None if no selection)
    pub selection_start: Option<usize>,
    /// Whether content has changed since last sync
    pub dirty: bool,
    /// CRDT version this content is based on
    pub version: u64,
    /// Pending text operations to send to server.
    pub pending_ops: Vec<EditOp>,
    /// Pending block operations to send to server.
    pub pending_block_ops: Vec<BlockOp>,
}

impl Default for CellEditor {
    fn default() -> Self {
        Self {
            blocks: Vec::new(),
            cursor_block: 0,
            cursor_offset: 0,
            text_cache: String::new(),
            cursor: 0,
            selection_start: None,
            dirty: false,
            version: 0,
            pending_ops: Vec::new(),
            pending_block_ops: Vec::new(),
        }
    }
}

impl CellEditor {
    pub fn new() -> Self {
        Self::default()
    }

    /// Builder: set initial text content.
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.set_text(text);
        self
    }

    /// Create an editor with content blocks.
    pub fn with_blocks(mut self, blocks: Vec<ContentBlock>) -> Self {
        self.blocks = blocks;
        self.sync_text_from_blocks();
        self
    }

    /// Builder: set CRDT version.
    pub fn with_version(mut self, version: u64) -> Self {
        self.version = version;
        self
    }

    // =========================================================================
    // TEXT ACCESS & MUTATION
    // =========================================================================

    /// Get the full text content.
    ///
    /// For cells with blocks, this is the concatenated text from all blocks.
    /// For simple text cells, this is the raw text.
    pub fn text(&self) -> &str {
        &self.text_cache
    }

    /// Set the text content (CRDT-tracked).
    ///
    /// Creates a single Text block containing the content.
    /// This is tracked via BlockOp for CRDT sync.
    pub fn set_text(&mut self, text: impl Into<String>) {
        let text = text.into();
        let block = ContentBlock::Text(text);
        // Track as block operation for CRDT
        self.pending_block_ops.push(BlockOp::ReplaceAll {
            blocks: vec![block.clone()],
        });
        self.blocks = vec![block];
        self.sync_text_from_blocks();
        self.cursor = self.text_cache.len();
        self.dirty = true;
    }

    /// Apply server-authoritative content.
    ///
    /// This is used when receiving state from the server. It updates local
    /// content without creating outbound CRDT operations (to avoid feedback loops).
    /// The server is the source of truth in this case.
    pub fn apply_server_content(&mut self, content: impl Into<String>) {
        let text = content.into();
        self.blocks = vec![ContentBlock::Text(text)];
        self.sync_text_from_blocks();
        self.cursor = self.text_cache.len();
        // Note: dirty is NOT set since this is server-authoritative
    }

    /// Clear all content.
    pub fn clear(&mut self) {
        self.blocks.clear();
        self.text_cache.clear();
        self.cursor = 0;
        self.cursor_block = 0;
        self.cursor_offset = 0;
        self.dirty = true;
    }

    // =========================================================================
    // BLOCK OPERATIONS
    // =========================================================================

    /// Replace all blocks (CRDT-tracked).
    pub fn replace_blocks(&mut self, blocks: Vec<ContentBlock>) {
        self.pending_block_ops.push(BlockOp::ReplaceAll {
            blocks: blocks.clone(),
        });
        self.blocks = blocks;
        self.sync_text_from_blocks();
        self.dirty = true;
    }

    /// Append a new block (CRDT-tracked).
    pub fn append_block(&mut self, block: ContentBlock) {
        let index = self.blocks.len();
        self.pending_block_ops.push(BlockOp::InsertBlock {
            index,
            block: block.clone(),
        });
        self.blocks.push(block);
        self.sync_text_from_blocks();
        self.dirty = true;
    }

    /// Update text within a specific block.
    pub fn update_block_text(&mut self, index: usize, new_text: String) {
        if index >= self.blocks.len() {
            return;
        }

        match &mut self.blocks[index] {
            ContentBlock::Text(text) => {
                *text = new_text;
            }
            ContentBlock::Thinking { text, .. } => {
                *text = new_text;
            }
            ContentBlock::ToolResult { content, .. } => {
                *content = new_text;
            }
            ContentBlock::ToolUse { .. } => {
                // Tool use blocks don't have editable text
            }
        }
        self.sync_text_from_blocks();
        self.dirty = true;
    }

    /// Toggle collapse state of a thinking block.
    pub fn toggle_block_collapse(&mut self, index: usize) {
        if let Some(ContentBlock::Thinking { collapsed, .. }) = self.blocks.get_mut(index) {
            *collapsed = !*collapsed;
            self.pending_block_ops.push(BlockOp::CollapseBlock {
                index,
                collapsed: *collapsed,
            });
            self.dirty = true;
        }
    }

    /// Check if the editor has content blocks.
    pub fn has_blocks(&self) -> bool {
        !self.blocks.is_empty()
    }

    /// Get block at cursor position.
    pub fn current_block(&self) -> Option<&ContentBlock> {
        self.blocks.get(self.cursor_block)
    }

    /// Sync the text field from blocks (for backward compatibility).
    fn sync_text_from_blocks(&mut self) {
        if self.blocks.is_empty() {
            return;
        }

        self.text_cache = self
            .blocks
            .iter()
            .filter_map(|b| b.text())
            .collect::<Vec<_>>()
            .join("\n\n");
        self.cursor = self.text_cache.len();
    }

    /// Take pending block operations for sending to server.
    pub fn take_pending_block_ops(&mut self) -> Vec<BlockOp> {
        std::mem::take(&mut self.pending_block_ops)
    }

    /// Check if there are pending block operations to send.
    pub fn has_pending_block_ops(&self) -> bool {
        !self.pending_block_ops.is_empty()
    }

    /// Insert text at cursor position.
    pub fn insert(&mut self, text: &str) {
        let pos = self.cursor;
        self.text_cache.insert_str(pos, text);
        self.cursor += text.len();
        self.dirty = true;
        self.pending_ops.push(EditOp::Insert {
            pos,
            text: text.to_string(),
        });
    }

    /// Delete character before cursor (backspace).
    pub fn backspace(&mut self) {
        if self.cursor > 0 {
            // Find the previous character boundary
            let mut new_cursor = self.cursor - 1;
            while new_cursor > 0 && !self.text_cache.is_char_boundary(new_cursor) {
                new_cursor -= 1;
            }
            let deleted_len = self.cursor - new_cursor;
            self.text_cache.drain(new_cursor..self.cursor);
            self.cursor = new_cursor;
            self.dirty = true;
            self.pending_ops.push(EditOp::Delete {
                pos: new_cursor,
                len: deleted_len,
            });
        }
    }

    /// Delete character at cursor (delete key).
    pub fn delete(&mut self) {
        if self.cursor < self.text_cache.len() {
            // Find the next character boundary
            let mut end = self.cursor + 1;
            while end < self.text_cache.len() && !self.text_cache.is_char_boundary(end) {
                end += 1;
            }
            let deleted_len = end - self.cursor;
            self.text_cache.drain(self.cursor..end);
            self.dirty = true;
            self.pending_ops.push(EditOp::Delete {
                pos: self.cursor,
                len: deleted_len,
            });
        }
    }

    /// Move cursor left.
    pub fn move_left(&mut self) {
        if self.cursor > 0 {
            self.cursor -= 1;
            while self.cursor > 0 && !self.text_cache.is_char_boundary(self.cursor) {
                self.cursor -= 1;
            }
        }
    }

    /// Move cursor right.
    pub fn move_right(&mut self) {
        if self.cursor < self.text_cache.len() {
            self.cursor += 1;
            while self.cursor < self.text_cache.len() && !self.text_cache.is_char_boundary(self.cursor) {
                self.cursor += 1;
            }
        }
    }

    /// Move cursor to start of line.
    pub fn move_home(&mut self) {
        // Find previous newline or start
        self.cursor = self.text_cache[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
    }

    /// Move cursor to end of line.
    pub fn move_end(&mut self) {
        // Skip current position if already at newline to avoid getting stuck
        let search_start = if self.text_cache.get(self.cursor..self.cursor + 1) == Some("\n") {
            self.cursor + 1
        } else {
            self.cursor
        };
        // Find next newline or end
        self.cursor = self.text_cache[search_start..]
            .find('\n')
            .map(|i| search_start + i)
            .unwrap_or(self.text_cache.len());
    }

    /// Get selected text, if any.
    ///
    /// Returns None if no selection or if bounds fall on invalid UTF-8 boundaries.
    pub fn selection(&self) -> Option<&str> {
        self.selection_start.and_then(|start| {
            let (a, b) = if start < self.cursor {
                (start, self.cursor)
            } else {
                (self.cursor, start)
            };
            // Safe bounds check - prevents panic on multi-byte character boundaries
            self.text_cache.get(a..b)
        })
    }

    /// Mark as synced with given version.
    pub fn mark_synced(&mut self, version: u64) {
        self.dirty = false;
        self.version = version;
        self.pending_ops.clear();
    }

    /// Take pending operations for sending to server.
    pub fn take_pending_ops(&mut self) -> Vec<EditOp> {
        std::mem::take(&mut self.pending_ops)
    }

    /// Check if there are pending operations to send.
    pub fn has_pending_ops(&self) -> bool {
        !self.pending_ops.is_empty()
    }
}

/// Position of a cell in the workspace grid.
#[derive(Component, Default, Clone, Copy)]
pub struct CellPosition {
    /// Column (0-indexed)
    pub col: u32,
    /// Row (0-indexed)
    pub row: u32,
}

impl CellPosition {
    pub fn new(col: u32, row: u32) -> Self {
        Self { col, row }
    }
}

/// Visual state of a cell.
#[derive(Component, Default, Clone)]
pub struct CellState {
    /// Whether this cell is collapsed (children hidden)
    pub collapsed: bool,
    /// Computed height based on content (in pixels)
    pub computed_height: f32,
    /// Minimum height for the cell
    pub min_height: f32,
}

impl CellState {
    pub fn new() -> Self {
        Self {
            collapsed: false,
            computed_height: 100.0,
            min_height: 60.0,
        }
    }

    pub fn toggle_collapse(&mut self) {
        self.collapsed = !self.collapsed;
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

    pub fn color_hint(&self) -> &'static str {
        match self {
            EditorMode::Normal => "blue",
            EditorMode::Insert => "green",
            EditorMode::Command => "yellow",
            EditorMode::Visual => "purple",
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
    /// Cell width in pixels
    pub cell_width: f32,
    /// Minimum cell height
    pub min_cell_height: f32,
    /// Maximum cell height (0 = unlimited)
    pub max_cell_height: f32,
    /// Margin between cells
    pub cell_margin: f32,
    /// Left margin for the workspace
    pub workspace_margin_left: f32,
    /// Top margin for the workspace
    pub workspace_margin_top: f32,
    /// Line height for computing dynamic heights
    pub line_height: f32,
    /// Height of the draggable header area at the top of each cell
    pub drag_header_height: f32,
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
            cell_width: 700.0,
            min_cell_height: 60.0,
            max_cell_height: 400.0,
            cell_margin: 12.0,
            workspace_margin_left: 20.0,
            workspace_margin_top: 70.0, // Space for compact header
            line_height: 20.0,
            drag_header_height: 30.0,
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
