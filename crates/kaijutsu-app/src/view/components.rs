//! Cell components for Bevy ECS.
//!
//! Cells are the fundamental content primitive in Kaijutsu. Each cell contains
//! structured content blocks (text, thinking, tool use/results) managed by CRDTs.

use bevy::prelude::*;

// Re-export vocabulary types for convenience
pub use kaijutsu_types::{BlockId, BlockKind, BlockSnapshot, ContentType, DriftKind, Role, Status};
pub use kaijutsu_types::{ContextId, PrincipalId};

use crate::view::render_store::RenderBlockStore;

/// Session-scoped agent identity.
///
/// Created once at startup, reused for the `CellEditor` render buffer's
/// local `RenderBlockStore`, and — since Lane C slice 3 — for finding this
/// principal's own compose draft among the ordinary blocks in a context's
/// `ContextMirror` (`DocumentEntry::draft_text`). Without this, each frame
/// or context switch would generate a fresh PrincipalId, fragmenting block
/// authorship.
#[derive(Resource)]
pub struct SessionPrincipal(pub PrincipalId);

impl Default for SessionPrincipal {
    fn default() -> Self {
        Self(PrincipalId::new())
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

// ============================================================================
// CURSOR TYPES
// ============================================================================

/// Cursor position within a block document.
#[derive(Debug, Clone, Default, Reflect)]
#[allow(dead_code)]
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

/// Text editor state for a cell.
///
/// The `store` field is a `RenderBlockStore` (`view::render_store`) — the
/// local render buffer for this cell's blocks. Synced content arrives via
/// `DocumentCache` (a `ContextMirror` per context, docs/change-feed.md —
/// plain `BlockSnapshot`s, no CRDT) and is materialized into this editor's
/// store via `insert_from_snapshot`
/// (`view::sync::sync_main_cell_to_conversation`).
///
/// Note: Not reflectable due to `RenderBlockStore` lacking Default.
/// Use query filters to find CellEditor entities instead of BRP inspection.
#[derive(Component)]
#[allow(dead_code)]
pub struct CellEditor {
    /// Block store - local render buffer.
    pub store: RenderBlockStore,

    /// Cursor position within the document.
    pub cursor: BlockCursor,
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
            store: RenderBlockStore::new(ContextId::new(), PrincipalId::new()),
            cursor: BlockCursor::default(),
        }
    }

    /// Builder: set initial text content (creates a single text block).
    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        let text = text.into();
        if !text.is_empty()
            && let Ok(block_id) = self.store.insert_block(
                None,
                None,
                Role::User,
                BlockKind::Text,
                &text,
                Status::Done,
                ContentType::Plain,
            )
        {
            self.cursor = BlockCursor::at(block_id, text.len());
        }
        self
    }

    // =========================================================================
    // TEXT ACCESS
    // =========================================================================

    /// Get the full text content (concatenation of all blocks).
    #[allow(dead_code)]
    pub fn text(&self) -> String {
        self.store.full_text()
    }

    /// Get the current document version.
    pub fn version(&self) -> u64 {
        self.store.version()
    }

    /// Check if the editor has any blocks.
    #[allow(dead_code)]
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

    /// Get a single block snapshot (clones one block's content — the
    /// per-row seed path for `ConversationGeometry`, and the per-entity
    /// fetch that replaces whole-document `blocks()` clones).
    pub fn block_snapshot(&self, id: &BlockId) -> Option<BlockSnapshot> {
        self.store.get_block_snapshot(id)
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

// ============================================================================
// CONVERSATION UI LAYOUT COMPONENTS
// ============================================================================

/// Marker for the scrollable conversation container.
/// Holds message cells (UserMessage, AgentMessage, tool calls).
#[derive(Component, Reflect)]
#[reflect(Component)]
pub struct ConversationContainer;

/// Which edge of the conversation column a spacer occupies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Reflect)]
pub enum SpacerEdge {
    Top,
    Bottom,
}

/// Marker for one of the two spacer nodes bracketing a `ConversationContainer`'s
/// virtualized children.
///
/// `virtualize_conversation` removes offscreen block/header nodes from taffy
/// layout via `Node.display = Display::None`; the spacers' `Node.height`
/// stands in for the removed space so `content_height`/`ScrollPosition.y`
/// stay correct. Exactly one `Top` and one `Bottom` spacer exist per
/// `ConversationContainer`, always the first and last child respectively —
/// see `reorder_conversation_children` and `lifecycle::ensure_conversation_spacers`.
#[derive(Component, Reflect)]
#[reflect(Component)]
pub struct ConversationSpacer {
    pub edge: SpacerEdge,
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
/// (Escape), text stays in the draft block (Status::Draft + ephemeral) for
/// recall.
///
/// Think rofi/dmenu:
/// summon → orient (mode ring) → act → gone.
#[derive(Component, Reflect, Default)]
#[reflect(Component)]
pub struct InputOverlay {
    /// Current text content.
    pub text: String,
    /// Cursor position within the text (byte offset).
    pub cursor: usize,
    /// Selection anchor (byte offset). When Some, selection spans anchor..cursor.
    pub selection_anchor: Option<usize>,
    /// Current input mode (chat vs shell).
    pub mode: InputMode,
    /// Vim mode display string (e.g. "-- INSERT --", None = Normal).
    /// Updated by vim_dispatch_compose each frame.
    pub vim_mode: Option<String>,
    /// Target context for this input (None = use active context).
    #[reflect(ignore)]
    #[allow(dead_code)] // Floating-chat context targeting; submit path not yet reading it.
    pub target_context: Option<ContextId>,
}

impl InputOverlay {
    /// Get the selected range (ordered start..end), or None if no selection.
    pub fn selection_range(&self) -> Option<std::ops::Range<usize>> {
        self.selection_anchor.map(|anchor| {
            let start = anchor.min(self.cursor);
            let end = anchor.max(self.cursor);
            start..end
        })
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

    /// Check if the overlay is empty.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// Check if current mode is shell.
    pub fn is_shell(&self) -> bool {
        matches!(self.mode, InputMode::Shell)
    }

    /// Build the display text (raw input, no mode prefix — mode is in the dock).
    pub fn display_text(&self) -> &str {
        &self.text
    }

    /// Byte offset of the cursor within display_text.
    pub fn display_cursor_offset(&self) -> usize {
        self.cursor
    }
}

/// Marker for the input overlay entity.
#[derive(Component)]
pub struct InputOverlayMarker;

/// Marker for the MSDF text surface child of InputOverlay.
#[derive(Component)]
pub struct MsdfOverlayText;

/// Cached cursor position computed during glyph layout.
///
/// Written by `build_overlay_glyphs`, read by `update_overlay_cursor`.
/// Avoids re-running Parley layout in the cursor rendering system.
#[derive(Component, Default)]
pub struct OverlayCursorGeometry {
    /// Cursor X position in content-box pixels (includes border padding).
    pub x: f64,
    /// Cursor Y position (top of beam) in content-box pixels.
    pub y: f64,
    /// Cursor beam height in pixels.
    pub height: f64,
    /// Last cursor byte offset used for geometry — lets the glyph system
    /// detect cursor-only changes without re-running Parley layout.
    pub last_cursor_offset: usize,
    /// Cursor shape kind, derived from the parent overlay's `vim_mode`.
    pub kind: crate::input::vim::CursorKind,
    /// Visual-mode selection rect (content-box pixels). Width=0 means
    /// no selection. Single-line scope; multi-line falls back to no
    /// rect for now.
    pub selection_x: f64,
    pub selection_y: f64,
    pub selection_width: f64,
    pub selection_height: f64,
    /// Anchor byte offset used to compute the selection rect — lets the
    /// glyph system detect anchor-only changes without re-running layout.
    pub last_selection_anchor: Option<usize>,
}

/// Marker for the main conversation view cell.
///
/// NOTE: MainCell no longer renders directly - it holds the CellEditor
/// (source of truth for content) while BlockCells handle per-block rendering.
/// Kept as the "owner" entity for BlockCellContainer and TurnCellContainer.
#[derive(Component, Reflect)]
#[reflect(Component)]
pub struct MainCell;

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
/// Emitted by context-switch affordances (gt/gT/Ctrl-^/click) and the
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
    /// Set when new block entities are spawned this frame.
    /// Consumed by readback_block_heights (PostUpdate) to compute the scroll anchor.
    pub new_blocks_added: bool,
    /// When new blocks are added, this holds the content height *before* the new
    /// blocks were measured. smooth_scroll uses min(max, anchor) so the viewport
    /// reveals new content from its start rather than jumping to its bottom.
    /// Cleared after one smooth_scroll consumption.
    pub pending_scroll_anchor: Option<f32>,
}

impl Default for ConversationScrollState {
    fn default() -> Self {
        Self {
            offset: 0.0,
            target_offset: 0.0,
            content_height: 0.0,
            visible_height: 600.0, // Will be updated by layout system
            following: true,       // Start in follow mode
            new_blocks_added: false,
            pending_scroll_anchor: None,
        }
    }
}

impl ConversationScrollState {
    /// True once `target_offset` has actually reached the TRUE bottom
    /// (within ~1px). Used everywhere a scroll re-latches follow mode
    /// (`scroll_by`, `input::systems::scroll_to_rect_visible`) to decide
    /// whether a downward scroll may opt back in: sticky-follow semantics
    /// (2026-08-16, scroll-relief slice 0) require actually reaching the
    /// tail, not merely coming close to it — the invariant has to hold at
    /// every re-latch site.
    ///
    /// This used to coexist with a looser `is_at_bottom()` (a 50px "near the
    /// bottom" band), and one re-latch site was still built on that looser
    /// method — the exact backdoor the invariant above warns about (found by
    /// kaibo/qwen review, 2026-08-16: `scroll_to_rect_visible` block
    /// navigation landing 40px from the true bottom was re-latching follow).
    /// `is_at_bottom()` is gone now: once every re-latch site used
    /// `reached_bottom()`, it had no callers left, and keeping a second,
    /// looser "at bottom" predicate around is exactly what let the bug
    /// happen twice.
    pub(crate) fn reached_bottom(&self) -> bool {
        const EPS: f32 = 1.0;
        self.target_offset >= self.max_offset() - EPS
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
    /// Moves only the TARGET — `smooth_scroll` eases the visible `offset`
    /// toward it over subsequent frames, so wheel input glides instead of
    /// teleporting.
    ///
    /// Follow mode is sticky (2026-08-16, scroll-relief slice 0): once the
    /// user scrolls away from the tail, `following` stays OFF until they
    /// explicitly return — a downward scroll back to the true bottom here,
    /// or an explicit `ScrollToEnd`/`start_following` action. New content
    /// arriving must never re-latch it by itself; `handle_block_events`
    /// (`view/sync.rs`) used to re-enable follow from a 50px "near the
    /// bottom" band whenever layout advanced, which was the yank bug: scroll
    /// up <50px to read, pause a frame, and the next streamed block silently
    /// snapped the view back to the bottom.
    pub fn scroll_by(&mut self, delta: f32) {
        // If scrolling up, disable follow mode
        if delta < 0.0 {
            self.following = false;
        }

        self.target_offset += delta;
        self.clamp_target();

        // If scrolling DOWN and target lands at the TRUE bottom (within
        // ~1px — `reached_bottom`, not the 50px `is_at_bottom()` band),
        // re-enable follow mode. The `delta > 0.0` guard is load-bearing:
        // without it, a small upward scroll that stays within a "near the
        // bottom" band would re-enable follow on the very same call that
        // just disabled it (above), and the next frame would snap back to
        // the bottom. Only a downward scroll that actually reaches bottom
        // should re-follow — "8 swipes before it responds; a flick zips"
        // (MX Master 3, 2026-07-18) is the regression a looser band caused.
        if delta > 0.0 && self.reached_bottom() {
            self.following = true;
            self.clear_stale_scroll_anchor();
        }
    }

    /// Set target to bottom and enable follow mode.
    pub fn scroll_to_end(&mut self) {
        self.target_offset = self.max_offset();
        self.following = true;
        self.clear_stale_scroll_anchor();
    }

    /// Enable follow mode (will smoothly scroll to and track bottom).
    pub fn start_following(&mut self) {
        self.following = true;
        self.clear_stale_scroll_anchor();
    }

    /// Drop a leftover `pending_scroll_anchor` when explicitly (re-)engaging
    /// follow (found by kaibo/qwen review, 2026-08-16). `readback_block_heights`
    /// (`view/render.rs`) unconditionally sets the anchor whenever new blocks
    /// are added, but `smooth_scroll` only ever drains it while `following`
    /// is true — so blocks streaming in while the user is scrolled away
    /// leave a stale anchor sitting here (repeatedly overwritten, never
    /// consumed). Without this, the *next* time the user explicitly returns
    /// to the bottom, `smooth_scroll` would consume that stale anchor and
    /// land them at `anchor.min(max)` — partway through the conversation,
    /// not the bottom they just asked for (they'd have to press it twice).
    /// Every site that flips `following` on from an explicit user action
    /// calls this; the streaming-while-following case is unaffected because
    /// `smooth_scroll` drains a fresh anchor the same frame it's set.
    fn clear_stale_scroll_anchor(&mut self) {
        self.pending_scroll_anchor = None;
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
// GLOBAL ERROR QUEUE (context-free RPC errors)
// ============================================================================

/// Transient error queue for context-free failures (e.g., create_context
/// failure, kernel attach failure). These can't be CRDT-synced because
/// there's no context to sync them into.
///
/// The dock HUD renders these as toasts that auto-dismiss.
#[derive(Resource, Default)]
pub struct GlobalErrorQueue {
    pub entries: std::collections::VecDeque<GlobalError>,
}

/// A single transient error entry for dock HUD display.
pub struct GlobalError {
    pub operation: String,
    pub message: String,
    pub created_at: f64,
    /// Number of times this exact `(operation, message)` pair has recurred
    /// since it first appeared. A reconnect storm (e.g. "no SSH agent" fired
    /// once per retry) collapses into this counter instead of consuming a
    /// new slot each time — see `push`.
    pub repeat_count: u32,
}

impl GlobalErrorQueue {
    /// Record an error. An exact repeat of an entry already in the queue
    /// (same `operation` AND `message`, anywhere in the queue, not just the
    /// most recent) bumps that entry's `repeat_count` and refreshes its
    /// `created_at` (so a still-recurring problem doesn't quietly age out)
    /// instead of pushing a new entry.
    ///
    /// This exists specifically so a cascade of IDENTICAL errors — a
    /// reconnect storm hammering the same failure once per attempt — cannot
    /// evict a distinct, earlier, still-relevant error: duplicates never
    /// consume a slot. Only genuinely distinct errors compete for the 5
    /// slots, and among those, oldest-first FIFO is the eviction rule (a
    /// transient toast queue is not meant to pin the first error forever).
    pub fn push(&mut self, operation: impl Into<String>, message: impl Into<String>, time: f64) {
        let operation = operation.into();
        let message = message.into();

        if let Some(existing) = self
            .entries
            .iter_mut()
            .find(|e| e.operation == operation && e.message == message)
        {
            existing.repeat_count = existing.repeat_count.saturating_add(1);
            existing.created_at = time;
            return;
        }

        self.entries.push_back(GlobalError {
            operation,
            message,
            created_at: time,
            repeat_count: 1,
        });
        // Keep at most 5 visible DISTINCT errors.
        while self.entries.len() > 5 {
            self.entries.pop_front();
        }
    }

    /// Remove errors older than `max_age_secs`.
    pub fn gc(&mut self, now: f64, max_age_secs: f64) {
        self.entries.retain(|e| now - e.created_at < max_age_secs);
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
    pub last_render_version: Option<u64>,
    /// Last known text length for layout dirty detection.
    /// Word-wrap line count can only change when text length changes,
    /// so this catches wrapping that newline-count missed.
    pub last_text_len: usize,
    /// Last known block status for border dirty detection.
    /// Status changes (Running→Done) affect border kind/animation.
    pub last_status: kaijutsu_types::Status,
    /// Last known rainbow effect state for change detection.
    pub last_rainbow: bool,
}

impl BlockCell {
    pub fn new(block_id: BlockId) -> Self {
        Self {
            block_id,
            last_render_version: None,
            last_text_len: 0,
            last_status: kaijutsu_types::Status::Running,
            last_rainbow: false,
        }
    }
}

/// Container that tracks all BlockCell entities for a conversation view.
///
/// Uses an IndexMap to maintain insertion order while providing O(1) lookup.
/// Attached to the entity that owns the conversation display (e.g., MainCell parent).
#[derive(Component, Debug, Default, Reflect)]
#[reflect(Component)]
pub struct BlockCellContainer {
    /// Ordered map from block ID to entity — single source of truth.
    #[reflect(ignore)]
    pub block_cells: indexmap::IndexMap<BlockId, Entity>,
    /// Role header entities (one per role transition).
    pub role_headers: Vec<Entity>,
}

impl BlockCellContainer {
    /// Add a new block cell.
    pub fn add(&mut self, block_id: BlockId, entity: Entity) {
        self.block_cells.insert(block_id, entity);
    }

    /// Remove a block cell by entity.
    pub fn remove(&mut self, entity: Entity) {
        self.block_cells.retain(|_, e| *e != entity);
    }

    /// Get entity for a block ID.
    pub fn get_entity(&self, block_id: &BlockId) -> Option<Entity> {
        self.block_cells.get(block_id).copied()
    }

    /// Check if a block ID is already tracked.
    pub fn contains(&self, block_id: &BlockId) -> bool {
        self.block_cells.contains_key(block_id)
    }

    /// Iterate over entities in order.
    pub fn entities(&self) -> impl Iterator<Item = Entity> + '_ {
        self.block_cells.values().copied()
    }

    /// Re-sort `block_cells` to match `current_blocks` (document order).
    ///
    /// Returns `true` if the sort actually changed key order. A pure
    /// position change — a server `BlockMoved`, or a merge that repositions
    /// an existing block without adding/removing any — must still surface
    /// as "order changed" so the caller can bump `LayoutGeneration`.
    /// Previously this sort ran unconditionally but nothing downstream
    /// noticed unless a block was also added or removed, so
    /// `reorder_conversation_children` never re-ran and the visual order
    /// went stale until app restart.
    pub fn resort_to_document_order(&mut self, current_blocks: &[BlockId]) -> bool {
        let before: Vec<BlockId> = self.block_cells.keys().copied().collect();

        let order: std::collections::HashMap<&BlockId, usize> = current_blocks
            .iter()
            .enumerate()
            .map(|(i, id)| (id, i))
            .collect();
        self.block_cells.sort_by(|a, _, b, _| {
            let a_idx = order.get(a).copied().unwrap_or(usize::MAX);
            let b_idx = order.get(b).copied().unwrap_or(usize::MAX);
            a_idx.cmp(&b_idx)
        });

        let after: Vec<BlockId> = self.block_cells.keys().copied().collect();
        before != after
    }
}

/// Computed layout for a block cell.
#[derive(Component, Debug, Default, Reflect)]
#[reflect(Component)]
pub struct BlockCellLayout {
    /// Y position (top) relative to conversation content start.
    pub y_offset: f32,
    /// Computed height based on content. Cached from the last frame this
    /// block was actually laid out (`Display::Flex`) — kept as-is while the
    /// block is virtualized out (`Display::None`) so the logical geometry
    /// model stays valid without a live taffy measurement.
    pub height: f32,
    /// Indentation level (for nested tool results).
    pub indent_level: u32,
    /// `BlockCell.last_render_version` at the time `height` was last
    /// measured from `ComputedNode`. A mirror of `GeomRow.measured_version`
    /// for consumers that already hold the entity — the geometry row is the
    /// source of truth, and virtualization decides purely on the window (see
    /// `virtualize_conversation`), never on this staleness signal.
    pub last_measured_version: u64,
}

// ============================================================================
// ROLE GROUP BORDER COMPONENTS
// ============================================================================

/// Role group border entity that appears before first block of each turn.
/// Rendered via the shared `BlockFxMaterial`/MSDF block pipeline: a
/// `BorderKind::CenterLine` shader rule with an MSDF role label straddling a
/// gap in it (see `view::block_render::sync_role_group_headers`).
///
/// Replaces the old text-based RoleHeader ("── USER ──────────").
///
/// Note: Not fully reflectable due to BlockId lacking Default.
#[derive(Component, Debug, Clone)]
pub struct RoleGroupBorder {
    /// The role this header represents.
    pub role: kaijutsu_types::Role,
    /// The block ID this header precedes (for layout positioning).
    pub block_id: BlockId,
}

/// Layout information for a role group border.
#[derive(Component, Debug, Default, Reflect)]
#[reflect(Component)]
pub struct RoleGroupBorderLayout {
    /// Y position (top) relative to conversation content start.
    pub y_offset: f32,
    /// Computed height based on content. Same caching contract as
    /// `BlockCellLayout::height` — held over while `Display::None`.
    pub height: f32,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_block_id(seq: u64) -> BlockId {
        BlockId::new(ContextId::new(), PrincipalId::new(), seq)
    }

    #[test]
    fn resort_to_document_order_reports_no_change_when_already_sorted() {
        let ids: Vec<BlockId> = (0..3).map(test_block_id).collect();
        let mut container = BlockCellContainer::default();
        for id in &ids {
            container.add(*id, Entity::PLACEHOLDER);
        }
        assert!(!container.resort_to_document_order(&ids));
    }

    #[test]
    fn resort_to_document_order_reports_change_on_pure_reorder() {
        // Container built in insertion order A, B, C — then the document
        // order moves C to the front (a BlockMoved / merge reposition with
        // no additions or removals). This is the exact case that used to
        // slip past spawn_block_cells's bump gate.
        let ids: Vec<BlockId> = (0..3).map(test_block_id).collect();
        let mut container = BlockCellContainer::default();
        for id in &ids {
            container.add(*id, Entity::PLACEHOLDER);
        }

        let document_order = vec![ids[2], ids[0], ids[1]];
        assert!(container.resort_to_document_order(&document_order));

        let after: Vec<BlockId> = container.block_cells.keys().copied().collect();
        assert_eq!(after, document_order);
    }

    #[test]
    fn resort_to_document_order_is_idempotent() {
        let ids: Vec<BlockId> = (0..3).map(test_block_id).collect();
        let mut container = BlockCellContainer::default();
        for id in &ids {
            container.add(*id, Entity::PLACEHOLDER);
        }
        let document_order = vec![ids[1], ids[2], ids[0]];
        assert!(container.resort_to_document_order(&document_order));
        // Second call against the same target order changes nothing further.
        assert!(!container.resort_to_document_order(&document_order));
    }

    fn scroll_state(
        content_height: f32,
        visible_height: f32,
        target_offset: f32,
    ) -> ConversationScrollState {
        ConversationScrollState {
            offset: target_offset,
            target_offset,
            content_height,
            visible_height,
            following: false,
            new_blocks_added: false,
            pending_scroll_anchor: None,
        }
    }

    #[test]
    fn test_reached_bottom_at_max() {
        let state = scroll_state(1000.0, 400.0, 600.0);
        assert!(state.reached_bottom());
    }

    #[test]
    fn test_reached_bottom_within_the_1px_threshold() {
        // max_offset=600, target=599.5 → 600-599.5=0.5 < 1.0
        let state = scroll_state(1000.0, 400.0, 599.5);
        assert!(state.reached_bottom());
    }

    #[test]
    fn test_reached_bottom_outside_the_1px_threshold() {
        // max_offset=600, target=598.9 → 600-598.9=1.1 > 1.0
        let state = scroll_state(1000.0, 400.0, 598.9);
        assert!(!state.reached_bottom());
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
    fn test_scroll_by_small_up_near_bottom_still_disables_following() {
        // Regression (2026-07-18): a SMALL upward scroll while inside the 50px
        // is_at_bottom() band must break follow. The old code re-enabled follow
        // unconditionally when is_at_bottom(), so small high-res wheel steps
        // near the bottom did nothing until ~50px accumulated in one frame
        // ("8 swipes before it responds; a flick zips"). The big-delta sibling
        // test (-100) escapes the band and never caught this.
        let mut state = scroll_state(1000.0, 400.0, 600.0); // at the bottom
        state.following = true;
        state.scroll_by(-10.0); // still within 50px of max
        assert!(
            !state.following,
            "small upward scroll near the bottom must disable follow"
        );
        assert_eq!(state.target_offset, 590.0, "target moved up");
    }

    #[test]
    fn test_scroll_by_positive_to_bottom_re_enables_following() {
        let mut state = scroll_state(1000.0, 400.0, 590.0);
        state.following = false;
        state.scroll_by(100.0); // would go past max, gets clamped
        assert!(state.following);
        assert_eq!(state.target_offset, 600.0, "clamped to max");
    }

    /// Sticky-follow regression (2026-08-16, scroll-relief slice 0): landing
    /// merely *inside* the 50px `is_at_bottom()` band must NOT re-enable
    /// follow — only actually reaching the true bottom (within ~1px) should.
    /// Before this fix `scroll_by` re-latched off `is_at_bottom()`, so a
    /// downward nudge that only closed *part* of the gap could silently
    /// re-arm follow well before the user was really back at the tail.
    #[test]
    fn test_scroll_by_positive_within_band_but_not_at_bottom_stays_off() {
        let mut state = scroll_state(1000.0, 400.0, 500.0); // max=600, 100px up
        state.following = false;
        state.scroll_by(60.0); // target=560: inside the old 50px band, not at bottom
        assert!(
            !state.following,
            "reaching only 40px from the bottom must not re-latch follow"
        );
        assert_eq!(state.target_offset, 560.0);
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

    /// Stale-anchor regression (found by kaibo/qwen review, 2026-08-16):
    /// `readback_block_heights` (`view/render.rs`) sets `pending_scroll_anchor`
    /// whenever new blocks are added, regardless of `following` — but
    /// `smooth_scroll` only ever drains it while following. Blocks streaming
    /// in while the user is scrolled away leave a stale anchor sitting here.
    /// Without clearing it, the next explicit "go to bottom" would have
    /// `smooth_scroll` consume that stale anchor and land the user
    /// mid-conversation instead of at the true bottom.
    #[test]
    fn test_scroll_to_end_clears_a_stale_pending_scroll_anchor() {
        let mut state = scroll_state(1000.0, 400.0, 0.0);
        state.pending_scroll_anchor = Some(123.0); // stale: set while not following
        state.scroll_to_end();
        assert_eq!(
            state.pending_scroll_anchor, None,
            "an explicit return to bottom must not honor a leftover anchor"
        );
    }

    #[test]
    fn test_start_following_clears_a_stale_pending_scroll_anchor() {
        let mut state = scroll_state(1000.0, 400.0, 300.0);
        state.pending_scroll_anchor = Some(456.0);
        state.start_following();
        assert_eq!(state.pending_scroll_anchor, None);
    }

    #[test]
    fn test_scroll_by_relatch_clears_a_stale_pending_scroll_anchor() {
        let mut state = scroll_state(1000.0, 400.0, 590.0); // max=600, near bottom
        state.pending_scroll_anchor = Some(789.0);
        state.following = false;
        state.scroll_by(100.0); // clamps to max_offset -> re-latches follow
        assert!(state.following);
        assert_eq!(state.pending_scroll_anchor, None);
    }

    // ------------------------------------------------------------------
    // GlobalErrorQueue — the dock HUD's data source for context-free errors
    // ------------------------------------------------------------------

    #[test]
    fn global_error_queue_starts_empty() {
        let queue = GlobalErrorQueue::default();
        assert!(queue.entries.is_empty());
    }

    #[test]
    fn global_error_queue_push_records_operation_message_and_time() {
        let mut queue = GlobalErrorQueue::default();
        queue.push("config", "theme.toml: bad TOML", 10.0);
        assert_eq!(queue.entries.len(), 1);
        let entry = &queue.entries[0];
        assert_eq!(entry.operation, "config");
        assert_eq!(entry.message, "theme.toml: bad TOML");
        assert_eq!(entry.created_at, 10.0);
        assert_eq!(entry.repeat_count, 1);
    }

    #[test]
    fn global_error_queue_caps_at_five_dropping_oldest() {
        let mut queue = GlobalErrorQueue::default();
        for i in 0..8 {
            queue.push("op", format!("error {i}"), i as f64);
        }
        assert_eq!(queue.entries.len(), 5);
        // Oldest three (0, 1, 2) were evicted; 3..=7 remain, in order.
        let messages: Vec<&str> = queue.entries.iter().map(|e| e.message.as_str()).collect();
        assert_eq!(messages, vec!["error 3", "error 4", "error 5", "error 6", "error 7"]);
    }

    #[test]
    fn global_error_queue_exact_repeat_bumps_count_instead_of_new_slot() {
        let mut queue = GlobalErrorQueue::default();
        queue.push("ssh", "no SSH agent", 0.0);
        queue.push("ssh", "no SSH agent", 1.0);
        queue.push("ssh", "no SSH agent", 2.0);

        assert_eq!(queue.entries.len(), 1, "identical repeats must not consume new slots");
        let entry = &queue.entries[0];
        assert_eq!(entry.repeat_count, 3);
        // Recency refreshes to the latest occurrence, so a still-recurring
        // problem doesn't quietly age out between retries.
        assert_eq!(entry.created_at, 2.0);
    }

    #[test]
    fn global_error_queue_reconnect_storm_of_identical_errors_does_not_evict_a_distinct_earlier_one() {
        // This is the exact scenario the review flagged: the diagnostic
        // FIRST error (theme.toml) must survive a burst of many identical
        // "no SSH agent" retries that would have filled a plain 5-slot FIFO
        // and evicted it.
        let mut queue = GlobalErrorQueue::default();
        queue.push("config", "theme.toml: bad TOML", 0.0);
        for i in 1..20 {
            queue.push("ssh", "no SSH agent", i as f64);
        }

        let messages: Vec<&str> = queue.entries.iter().map(|e| e.message.as_str()).collect();
        assert!(
            messages.contains(&"theme.toml: bad TOML"),
            "the distinct earlier error must still be present, got {messages:?}"
        );
        assert_eq!(queue.entries.len(), 2, "one distinct slot + one deduped repeat slot");
        let ssh_entry = queue.entries.iter().find(|e| e.operation == "ssh").unwrap();
        assert_eq!(ssh_entry.repeat_count, 19);
    }

    #[test]
    fn global_error_queue_distinct_burst_still_evicts_oldest_once_slots_are_full() {
        // A cascade of genuinely DISTINCT errors (not a repeat storm) still
        // follows plain FIFO once all 5 slots are in use — dedup only
        // protects against IDENTICAL repeats, not an unrelated cascade.
        let mut queue = GlobalErrorQueue::default();
        for i in 0..6 {
            queue.push("op", format!("distinct {i}"), i as f64);
        }
        assert_eq!(queue.entries.len(), 5);
        assert!(!queue.entries.iter().any(|e| e.message == "distinct 0"));
    }

    #[test]
    fn global_error_queue_gc_drops_entries_older_than_max_age() {
        let mut queue = GlobalErrorQueue::default();
        queue.push("op", "old", 0.0);
        queue.push("op", "recent", 8.0);
        // now = 10.0, max_age = 10.0 -> "old" (age 10.0) is not < 10.0, gone;
        // "recent" (age 2.0) survives.
        queue.gc(10.0, 10.0);
        assert_eq!(queue.entries.len(), 1);
        assert_eq!(queue.entries[0].message, "recent");
    }

    #[test]
    fn global_error_queue_gc_empties_out_once_everything_ages_past_max() {
        // The "aged out" end state the HUD badge relies on: once GC has run
        // long enough past every entry, the queue is fully empty again —
        // not just thinned.
        let mut queue = GlobalErrorQueue::default();
        queue.push("op", "a", 0.0);
        queue.push("op", "b", 1.0);
        queue.gc(100.0, 10.0);
        assert!(queue.entries.is_empty());
    }
}
