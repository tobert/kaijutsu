//! Cell components for Bevy ECS.
//!
//! Cells are the fundamental content primitive in Kaijutsu. Each cell contains
//! structured content blocks (text, thinking, tool use/results) managed by CRDTs.

use bevy::prelude::*;

// Re-export CRDT types for convenience
pub use kaijutsu_crdt::{BlockId, BlockKind, BlockSnapshot, DriftKind, Role, Status};
pub use kaijutsu_types::{ContextId, PrincipalId};

/// Session-scoped agent identity for CRDT operations.
///
/// Created once at startup, reused for all SyncedDocument
/// construction. Without this, each frame or context switch would generate
/// a fresh PrincipalId, fragmenting CRDT authorship and wasting DTE agent slots.
#[derive(Resource)]
pub struct SessionAgent(pub PrincipalId);

impl Default for SessionAgent {
    fn default() -> Self {
        Self(PrincipalId::new())
    }
}

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

/// Component linking a cell to a conversation.
///
/// When attached to a cell (like MainCell), the cell's content
/// is synced with the conversation's document in DocumentCache.
#[derive(Component, Debug, Clone, Reflect)]
#[reflect(Component)]
pub struct ViewingConversation {
    /// Context ID of the conversation this cell is viewing.
    #[reflect(ignore)]
    pub conversation_id: ContextId,
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
/// The `store` field (BlockStore) is the local editor buffer — one DTE instance
/// per block, matching the server's native format.
/// Synced content arrives via `DocumentCache` (SyncedDocument per context) and is
/// copied into this editor's BlockStore via `from_snapshot`.
///
/// Note: Not reflectable due to BlockStore lacking Default.
/// Use query filters to find CellEditor entities instead of BRP inspection.
#[derive(Component)]
pub struct CellEditor {
    /// Block store - local editor buffer (per-block DTE).
    pub store: kaijutsu_crdt::BlockStore,

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
        Self {
            store: kaijutsu_crdt::BlockStore::new(ContextId::new(), PrincipalId::new()),
            cursor: BlockCursor::default(),
            cursor_cache: CursorCache::default(),
        }
    }

    /// Builder: set initial text content (creates a single text block).
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        if !text.is_empty()
            && let Ok(block_id) = self.store.insert_block(None, None, Role::User, BlockKind::Text, &text) {
                self.cursor = BlockCursor::at(block_id, text.len());
            }
        self
    }

    // =========================================================================
    // TEXT ACCESS
    // =========================================================================

    /// Get the full text content (concatenation of all blocks).
    pub fn text(&self) -> String {
        self.store.full_text()
    }

    /// Get the current document version.
    pub fn version(&self) -> u64 {
        self.store.version()
    }

    /// Check if the editor has any blocks.
    pub fn has_blocks(&self) -> bool {
        !self.store.is_empty()
    }

    /// Get blocks in order.
    pub fn blocks(&self) -> Vec<BlockSnapshot> {
        self.store.blocks_ordered()
    }

    /// Get block IDs in order without constructing full snapshots.
    pub fn block_ids(&self) -> Vec<BlockId> {
        self.store.block_ids_ordered()
    }

    /// Toggle collapse state of a thinking block.
    pub fn toggle_block_collapse(&mut self, block_id: &BlockId) {
        if let Some(block) = self.store.get_block_snapshot(block_id) {
            let new_state = !block.collapsed;
            let _ = self.store.set_collapsed(block_id, new_state);
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


// EditorMode / CurrentMode replaced by input::FocusArea.
// Shell vs Chat auto-detected from compose text prefix.

// ============================================================================
// UNIFIED FOCUS RESOURCE
// ============================================================================

/// Unified focus tracking for keyboard focus and block navigation.
///
/// Consolidates the previous `FocusedCell` and `ConversationFocus` into a single
/// resource, eliminating confusion about which resource to check for focus state.
///
/// - `entity`: Which entity has keyboard focus (for cursor rendering, input routing)
/// - `block_id`: Which block is focused for j/k navigation and reply workflows
#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct FocusTarget {
    /// Entity with keyboard focus (for cursor rendering).
    pub entity: Option<Entity>,
    /// Block ID for navigation (j/k, reply workflows).
    #[reflect(ignore)]
    pub block_id: Option<BlockId>,
}

impl FocusTarget {
    /// Check if a specific block is focused.
    #[allow(dead_code)]
    pub fn is_block_focused(&self, block_id: &BlockId) -> bool {
        self.block_id.as_ref() == Some(block_id)
    }

    /// Clear all focus state.
    #[allow(dead_code)]
    pub fn clear(&mut self) {
        self.entity = None;
        self.block_id = None;
    }

    /// Set focus to a block (for j/k navigation).
    pub fn focus_block(&mut self, block_id: BlockId) {
        self.block_id = Some(block_id);
    }

    /// Set focus to an entity (for cursor/input).
    #[allow(dead_code)]
    pub fn focus_entity(&mut self, entity: Entity) {
        self.entity = Some(entity);
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
/// a full CellEditor - they render from the MainCell's BlockStore.
/// The cursor is an offset within the block's content string.
#[derive(Component, Default)]
pub struct BlockEditCursor {
    /// Byte offset within the block's content.
    pub offset: usize,
    /// Selection anchor (byte offset). When Some, selection spans anchor..offset.
    pub selection_anchor: Option<usize>,
    /// Per-block CRDT frontiers captured when editing started.
    /// Used to extract ops_since() for push to server on exit.
    pub edit_frontier: Option<std::collections::HashMap<BlockId, kaijutsu_crdt::Frontier>>,
}

// ============================================================================
// DOCUMENT CACHE — Multi-Context Document Management
// ============================================================================

/// A cached document for a single context, including its CRDT doc and sync state.
#[allow(dead_code)]
pub struct CachedDocument {
    /// Synced document — wraps CrdtBlockStore + SyncManager.
    pub synced: kaijutsu_client::SyncedDocument,
    /// CRDT-backed input document (compose scratchpad).
    /// `None` until the input state is fetched from the server.
    pub input: Option<kaijutsu_client::SyncedInput>,
    /// Context name (e.g. the kernel_id or user-supplied name).
    pub context_name: String,
    /// Generation counter at last sync (for staleness detection).
    pub synced_at_generation: u64,
    /// When this document was last accessed (for LRU eviction).
    pub last_accessed: std::time::Instant,
    /// Saved scroll offset (restored on switch-back).
    pub scroll_offset: f32,
}

/// Multi-context document cache — the authoritative source for all document state.
///
/// Holds a `SyncedDocument` per joined context, enabling:
/// - Instant context switching (cache hit → snapshot swap)
/// - Background sync for inactive contexts (events route by context_id)
/// - LRU eviction when too many contexts are cached
///
/// `sync_main_cell_to_conversation` reads from the active cache entry to
/// rebuild the MainCell's BlockStore for rendering.
#[derive(Resource)]
#[allow(dead_code)]
pub struct DocumentCache {
    /// Map from context_id → cached document state.
    documents: std::collections::HashMap<ContextId, CachedDocument>,
    /// Currently active (rendered) context_id.
    active_id: Option<ContextId>,
    /// Most-recently-used context IDs (front = most recent).
    mru: Vec<ContextId>,
    /// Maximum number of cached documents before LRU eviction.
    max_cached: usize,
}

impl Default for DocumentCache {
    fn default() -> Self {
        Self {
            documents: std::collections::HashMap::new(),
            active_id: None,
            mru: Vec::new(),
            max_cached: 8,
        }
    }
}

#[allow(dead_code)]
impl DocumentCache {
    /// Get the active context ID.
    pub fn active_id(&self) -> Option<ContextId> {
        self.active_id
    }

    /// Get a reference to a cached document by context_id.
    pub fn get(&self, context_id: ContextId) -> Option<&CachedDocument> {
        self.documents.get(&context_id)
    }

    /// Get a mutable reference to a cached document by context_id.
    pub fn get_mut(&mut self, context_id: ContextId) -> Option<&mut CachedDocument> {
        self.documents.get_mut(&context_id)
    }

    /// Check if a document is cached.
    pub fn contains(&self, context_id: ContextId) -> bool {
        self.documents.contains_key(&context_id)
    }

    /// Insert a new cached document. Evicts LRU entry if at capacity.
    pub fn insert(&mut self, context_id: ContextId, cached: CachedDocument) {
        if self.documents.len() >= self.max_cached {
            self.evict_lru();
        }
        self.documents.insert(context_id, cached);
        self.touch_mru(context_id);
    }

    /// Set the active document. Returns the previous active_id if changed.
    pub fn set_active(&mut self, context_id: ContextId) -> Option<ContextId> {
        let previous = self.active_id.take();
        self.active_id = Some(context_id);
        self.touch_mru(context_id);

        if let Some(doc) = self.documents.get_mut(&context_id) {
            doc.last_accessed = std::time::Instant::now();
        }

        previous
    }

    /// Get MRU-ordered context IDs (most recent first).
    pub fn mru_ids(&self) -> &[ContextId] {
        &self.mru
    }

    /// Number of cached documents.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Iterate over all cached documents.
    pub fn iter(&self) -> impl Iterator<Item = (ContextId, &CachedDocument)> {
        self.documents.iter().map(|(&k, v)| (k, v))
    }

    /// Move a context_id to the front of the MRU list.
    fn touch_mru(&mut self, context_id: ContextId) {
        self.mru.retain(|&id| id != context_id);
        self.mru.insert(0, context_id);
    }

    /// Evict the least-recently-used document (never the active one).
    fn evict_lru(&mut self) {
        let evict_id = self
            .mru
            .iter()
            .rev()
            .find(|&&id| self.active_id != Some(id))
            .copied();

        if let Some(id) = evict_id {
            self.documents.remove(&id);
            self.mru.retain(|&mid| mid != id);
            log::info!("DocumentCache: evicted LRU document {}", id);
        }
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
            line_height: 24.0, // Must match TextMetrics.cell_line_height for correct block sizing
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
    /// Cursor position within the text (byte offset)
    pub cursor: usize,
    /// Selection anchor (byte offset). When Some, selection spans anchor..cursor.
    pub selection_anchor: Option<usize>,
}

#[allow(dead_code)] // Phase 5: will be deleted when ComposeBlock is removed
impl ComposeBlock {
    /// Get the selected range (ordered start..end), or None if no selection.
    pub fn selection_range(&self) -> Option<std::ops::Range<usize>> {
        self.selection_anchor.map(|anchor| {
            let start = anchor.min(self.cursor);
            let end = anchor.max(self.cursor);
            start..end
        })
    }

    /// Get the selected text, or None if no selection.
    pub fn selected_text(&self) -> Option<&str> {
        self.selection_range().map(|r| &self.text[r])
    }

    /// Delete the current selection and return the deleted text.
    /// Cursor moves to start of deleted range.
    pub fn delete_selection(&mut self) -> Option<String> {
        let range = self.selection_range()?;
        let deleted: String = self.text[range.clone()].to_string();
        self.text.drain(range.clone());
        self.cursor = range.start;
        self.selection_anchor = None;
        Some(deleted)
    }

    /// Select all text.
    pub fn select_all(&mut self) {
        if !self.text.is_empty() {
            self.selection_anchor = Some(0);
            self.cursor = self.text.len();
        }
    }

    /// Clear selection without modifying text.
    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    /// Insert text at cursor position. Replaces selection if active.
    pub fn insert(&mut self, s: &str) {
        if self.selection_range().is_some() {
            self.delete_selection();
        }
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Delete character before cursor (backspace). Deletes selection if active.
    pub fn backspace(&mut self) {
        if self.selection_range().is_some() {
            self.delete_selection();
            return;
        }
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

    /// Delete character after cursor (delete). Deletes selection if active.
    pub fn delete(&mut self) {
        if self.selection_range().is_some() {
            self.delete_selection();
            return;
        }
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

    /// Move cursor left. Clears selection.
    pub fn move_left(&mut self) {
        self.clear_selection();
        if self.cursor > 0 {
            self.cursor = self.text[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    /// Move cursor right. Clears selection.
    pub fn move_right(&mut self) {
        self.clear_selection();
        if self.cursor < self.text.len() {
            self.cursor = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
        }
    }

    /// Clear and return the text (for submission).
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.selection_anchor = None;
        std::mem::take(&mut self.text)
    }

    /// Check if the compose block is empty.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}

// ============================================================================
// INPUT OVERLAY — Ephemeral input surface
// ============================================================================

/// Input mode for the overlay — determines submit routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Reflect)]
pub enum InputMode {
    /// Chat prompt — submitted as a user message.
    #[default]
    Chat,
    /// Shell command — submitted as a kaish invocation.
    Shell,
}

#[allow(dead_code)] // Phase 3: used by overlay systems
impl InputMode {
    /// Human-readable label for the mode ring indicator.
    pub fn label(&self) -> &'static str {
        match self {
            InputMode::Chat => "chat",
            InputMode::Shell => "shell",
        }
    }

    /// Cycle to the next mode in the ring.
    pub fn next(&self) -> Self {
        match self {
            InputMode::Chat => InputMode::Shell,
            InputMode::Shell => InputMode::Chat,
        }
    }
}

/// Ephemeral input overlay — summoned on demand, dismissed after use.
///
/// Text lives here temporarily while the overlay is visible. On submit,
/// text is routed through the existing `submit_input` RPC. On dismiss
/// (Escape), text stays in the CRDT InputDocEntry for recall.
///
/// This replaces the permanent ComposeBlock pane. Think rofi/dmenu:
/// summon → orient (mode ring) → act → gone.
#[derive(Component, Reflect, Default)]
#[reflect(Component)]
#[allow(dead_code)] // Phase 3: fields read by overlay systems
pub struct InputOverlay {
    /// Current text content.
    pub text: String,
    /// Cursor position within the text (byte offset).
    pub cursor: usize,
    /// Selection anchor (byte offset). When Some, selection spans anchor..cursor.
    pub selection_anchor: Option<usize>,
    /// Current input mode (chat vs shell).
    pub mode: InputMode,
    /// Target context for this input (None = use active context).
    #[reflect(ignore)]
    pub target_context: Option<ContextId>,
}

#[allow(dead_code)] // Phase 3: used by overlay input systems
impl InputOverlay {
    /// Get the selected range (ordered start..end), or None if no selection.
    pub fn selection_range(&self) -> Option<std::ops::Range<usize>> {
        self.selection_anchor.map(|anchor| {
            let start = anchor.min(self.cursor);
            let end = anchor.max(self.cursor);
            start..end
        })
    }

    /// Get the selected text, or None if no selection.
    pub fn selected_text(&self) -> Option<&str> {
        self.selection_range().map(|r| &self.text[r])
    }

    /// Delete the current selection and return the deleted text.
    pub fn delete_selection(&mut self) -> Option<String> {
        let range = self.selection_range()?;
        let deleted: String = self.text[range.clone()].to_string();
        self.text.drain(range.clone());
        self.cursor = range.start;
        self.selection_anchor = None;
        Some(deleted)
    }

    /// Select all text.
    pub fn select_all(&mut self) {
        if !self.text.is_empty() {
            self.selection_anchor = Some(0);
            self.cursor = self.text.len();
        }
    }

    /// Clear selection without modifying text.
    pub fn clear_selection(&mut self) {
        self.selection_anchor = None;
    }

    /// Insert text at cursor position. Replaces selection if active.
    pub fn insert(&mut self, s: &str) {
        if self.selection_range().is_some() {
            self.delete_selection();
        }
        self.text.insert_str(self.cursor, s);
        self.cursor += s.len();
    }

    /// Delete character before cursor (backspace). Deletes selection if active.
    pub fn backspace(&mut self) {
        if self.selection_range().is_some() {
            self.delete_selection();
            return;
        }
        if self.cursor > 0 {
            let prev = self.text[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
            self.text.drain(prev..self.cursor);
            self.cursor = prev;
        }
    }

    /// Delete character after cursor (delete). Deletes selection if active.
    pub fn delete(&mut self) {
        if self.selection_range().is_some() {
            self.delete_selection();
            return;
        }
        if self.cursor < self.text.len() {
            let next = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
            self.text.drain(self.cursor..next);
        }
    }

    /// Move cursor left. Clears selection.
    pub fn move_left(&mut self) {
        self.clear_selection();
        if self.cursor > 0 {
            self.cursor = self.text[..self.cursor]
                .char_indices()
                .last()
                .map(|(i, _)| i)
                .unwrap_or(0);
        }
    }

    /// Move cursor right. Clears selection.
    pub fn move_right(&mut self) {
        self.clear_selection();
        if self.cursor < self.text.len() {
            self.cursor = self.text[self.cursor..]
                .char_indices()
                .nth(1)
                .map(|(i, _)| self.cursor + i)
                .unwrap_or(self.text.len());
        }
    }

    /// Clear and return the text (for submission).
    pub fn take(&mut self) -> String {
        self.cursor = 0;
        self.selection_anchor = None;
        std::mem::take(&mut self.text)
    }

    /// Check if the overlay is empty.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Check if current mode is shell.
    pub fn is_shell(&self) -> bool {
        matches!(self.mode, InputMode::Shell)
    }

    /// Build the display text with mode ring prefix.
    pub fn display_text(&self) -> String {
        let mode_indicator = match self.mode {
            InputMode::Chat => "[chat] shell",
            InputMode::Shell => "chat [shell]",
        };
        if self.text.is_empty() {
            format!("{} │ ", mode_indicator)
        } else {
            format!("{} │ {}", mode_indicator, self.text)
        }
    }

    /// Byte offset of the cursor within display_text (accounting for prefix).
    pub fn display_cursor_offset(&self) -> usize {
        let prefix_len = match self.mode {
            InputMode::Chat => "[chat] shell │ ".len(),
            InputMode::Shell => "chat [shell] │ ".len(),
        };
        prefix_len + self.cursor
    }
}

/// Marker for the input overlay entity.
#[derive(Component)]
#[allow(dead_code)] // Phase 3: spawned by overlay system
pub struct InputOverlayMarker;

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

/// Message fired when prompt submission fails (e.g. disconnected).
///
/// Carries the original text so it can be restored to the compose block.
#[derive(Message)]
pub struct SubmitFailed {
    pub text: String,
    pub reason: String,
}

/// Marker component for compose blocks in error state.
///
/// Inserted when a submit fails, drives a red border flash animation.
/// Removed automatically after the animation completes.
#[derive(Component)]
pub struct ComposeError {
    pub started: std::time::Instant,
}

/// Message requesting a context switch.
///
/// Emitted by constellation navigation (gt/gT/Ctrl-^/click) and the
/// context strip widget. The `handle_context_switch` system processes
/// this to swap documents from the DocumentCache.
#[derive(Message, Clone, Debug)]
pub struct ContextSwitchRequested {
    /// The context to switch to.
    pub context_id: ContextId,
}

/// Resource tracking a pending context switch for cache-miss handling.
///
/// When a `ContextSwitchRequested` targets a context not yet in `DocumentCache`,
/// we spawn a new actor to join the context and store the target here.
/// Once `ContextJoined` arrives for the matching context, we auto-switch.
#[derive(Resource, Default)]
pub struct PendingContextSwitch(pub Option<ContextId>);

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
    /// Last known text length for layout dirty detection.
    /// Word-wrap line count can only change when text length changes,
    /// so this catches wrapping that newline-count missed.
    pub last_text_len: usize,
    /// Last known rainbow effect state for change detection.
    pub last_rainbow: bool,
}

impl BlockCell {
    pub fn new(block_id: BlockId) -> Self {
        Self {
            block_id,
            last_render_version: 0,
            last_text_len: 0,
            last_rainbow: false,
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
// ROLE GROUP BORDER COMPONENTS
// ============================================================================

/// Role group border entity that appears before first block of each turn.
/// Rendered as a Vello-drawn horizontal line with inset role label.
///
/// Replaces the old text-based RoleHeader ("── USER ──────────").
///
/// Note: Not fully reflectable due to BlockId lacking Default.
#[derive(Component, Debug, Clone)]
pub struct RoleGroupBorder {
    /// The role this header represents.
    pub role: kaijutsu_crdt::Role,
    /// The block ID this header precedes (for layout positioning).
    pub block_id: BlockId,
}

/// Layout information for a role group border.
#[derive(Component, Debug, Default, Reflect)]
#[reflect(Component)]
pub struct RoleGroupBorderLayout {
    /// Y position (top) relative to conversation content start.
    pub y_offset: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scroll_state(content_height: f32, visible_height: f32, target_offset: f32) -> ConversationScrollState {
        ConversationScrollState {
            offset: target_offset,
            target_offset,
            content_height,
            visible_height,
            following: false,
            user_scrolled_this_frame: false,
            last_content_gen: 0,
        }
    }

    #[test]
    fn test_is_at_bottom_at_max() {
        let state = scroll_state(1000.0, 400.0, 600.0);
        assert!(state.is_at_bottom());
    }

    #[test]
    fn test_is_at_bottom_within_threshold() {
        // 50px threshold: max_offset=600, target=551 → 600-551=49 < 50
        let state = scroll_state(1000.0, 400.0, 551.0);
        assert!(state.is_at_bottom());
    }

    #[test]
    fn test_is_at_bottom_outside_threshold() {
        // max_offset=600, target=549 → 600-549=51 > 50
        let state = scroll_state(1000.0, 400.0, 549.0);
        assert!(!state.is_at_bottom());
    }

    #[test]
    fn test_max_offset_content_smaller_than_visible() {
        let state = scroll_state(200.0, 400.0, 0.0);
        assert_eq!(state.max_offset(), 0.0);
    }

    #[test]
    fn test_scroll_by_negative_disables_following() {
        let mut state = scroll_state(1000.0, 400.0, 600.0);
        state.following = true;
        // Scroll up far enough to be outside the 50px threshold
        state.scroll_by(-100.0);
        assert!(!state.following);
    }

    #[test]
    fn test_scroll_by_positive_to_bottom_re_enables_following() {
        let mut state = scroll_state(1000.0, 400.0, 590.0);
        state.following = false;
        state.scroll_by(100.0); // would go past max, gets clamped
        assert!(state.following);
        assert_eq!(state.offset, 600.0); // clamped to max
    }

    #[test]
    fn test_scroll_by_sets_user_scrolled_flag() {
        let mut state = scroll_state(1000.0, 400.0, 300.0);
        assert!(!state.user_scrolled_this_frame);
        state.scroll_by(10.0);
        assert!(state.user_scrolled_this_frame);
    }

    #[test]
    fn test_start_following_enables_follow_mode() {
        let mut state = scroll_state(1000.0, 400.0, 300.0);
        assert!(!state.following);
        state.start_following();
        assert!(state.following);
    }

    #[test]
    fn test_scroll_to_end_sets_target_and_following() {
        let mut state = scroll_state(1000.0, 400.0, 0.0);
        state.scroll_to_end();
        assert_eq!(state.target_offset, 600.0);
        assert!(state.following);
    }
}

