//! Cell components for Bevy ECS.

use bevy::prelude::*;

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

/// A single edit operation for CRDT sync.
#[derive(Debug, Clone)]
pub enum EditOp {
    /// Insert text at position
    Insert { pos: usize, text: String },
    /// Delete characters at position
    Delete { pos: usize, len: usize },
}

/// Text editor state for a cell.
///
/// Wraps a cosmic-text Buffer that integrates with glyphon rendering.
/// We store the raw text here; the actual Buffer is managed per-frame
/// to avoid lifetime issues with FontSystem.
#[derive(Component)]
pub struct CellEditor {
    /// Current text content
    pub text: String,
    /// Cursor position (byte offset)
    pub cursor: usize,
    /// Selection start (byte offset, None if no selection)
    pub selection_start: Option<usize>,
    /// Whether content has changed since last sync
    pub dirty: bool,
    /// CRDT version this content is based on
    pub version: u64,
    /// Pending operations to send to server
    pub pending_ops: Vec<EditOp>,
}

impl Default for CellEditor {
    fn default() -> Self {
        Self {
            text: String::new(),
            cursor: 0,
            selection_start: None,
            dirty: false,
            version: 0,
            pending_ops: Vec::new(),
        }
    }
}

impl CellEditor {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.text = text.into();
        self.cursor = self.text.len();
        self
    }

    /// Insert text at cursor position.
    pub fn insert(&mut self, text: &str) {
        let pos = self.cursor;
        self.text.insert_str(pos, text);
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
            while new_cursor > 0 && !self.text.is_char_boundary(new_cursor) {
                new_cursor -= 1;
            }
            let deleted_len = self.cursor - new_cursor;
            self.text.drain(new_cursor..self.cursor);
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
        if self.cursor < self.text.len() {
            // Find the next character boundary
            let mut end = self.cursor + 1;
            while end < self.text.len() && !self.text.is_char_boundary(end) {
                end += 1;
            }
            let deleted_len = end - self.cursor;
            self.text.drain(self.cursor..end);
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
            while self.cursor > 0 && !self.text.is_char_boundary(self.cursor) {
                self.cursor -= 1;
            }
        }
    }

    /// Move cursor right.
    pub fn move_right(&mut self) {
        if self.cursor < self.text.len() {
            self.cursor += 1;
            while self.cursor < self.text.len() && !self.text.is_char_boundary(self.cursor) {
                self.cursor += 1;
            }
        }
    }

    /// Move cursor to start of line.
    pub fn move_home(&mut self) {
        // Find previous newline or start
        self.cursor = self.text[..self.cursor]
            .rfind('\n')
            .map(|i| i + 1)
            .unwrap_or(0);
    }

    /// Move cursor to end of line.
    pub fn move_end(&mut self) {
        // Skip current position if already at newline to avoid getting stuck
        let search_start = if self.text.get(self.cursor..self.cursor + 1) == Some("\n") {
            self.cursor + 1
        } else {
            self.cursor
        };
        // Find next newline or end
        self.cursor = self.text[search_start..]
            .find('\n')
            .map(|i| search_start + i)
            .unwrap_or(self.text.len());
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
            self.text.get(a..b)
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
}

impl Default for WorkspaceLayout {
    fn default() -> Self {
        Self {
            cell_width: 700.0,
            min_cell_height: 60.0,
            max_cell_height: 400.0,
            cell_margin: 12.0,
            workspace_margin_left: 20.0,
            workspace_margin_top: 120.0, // Space for header
            line_height: 20.0,
            drag_header_height: 30.0,
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
