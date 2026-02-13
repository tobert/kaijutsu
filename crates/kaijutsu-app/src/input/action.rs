//! Action enum — the universal vocabulary for user intent.
//!
//! Every input (key, gamepad button, MIDI CC, touch gesture) maps to an Action.
//! Domain systems consume `ActionFired` messages and never read raw input directly.

use bevy::prelude::*;

/// A discrete user action. Flat enum — no nesting, no ambiguity.
///
/// Actions are the bridge between input devices and domain logic.
/// The dispatcher maps raw input → Action; domain systems match on Action variants.
#[derive(Clone, Debug, PartialEq, Reflect)]
pub enum Action {
    // ========================================================================
    // Focus management
    // ========================================================================
    /// Tab — cycle focus forward through Compose → Conversation → Constellation → ...
    CycleFocusForward,
    /// Shift+Tab — cycle focus backward
    CycleFocusBackward,
    /// Shortcut to focus the compose area (i/Space in Navigation context)
    FocusCompose,
    /// Context-dependent "go up" (Escape)
    /// - TextInput → Conversation
    /// - Constellation → close, return to Conversation
    /// - Dialog → cancel
    Unfocus,
    /// Context-dependent "do the thing" (Enter)
    /// - Navigation: edit focused User Text block
    /// - Constellation: switch to focused context
    /// - Dialog: confirm
    /// - Dashboard: select
    Activate,

    // ========================================================================
    // Block navigation (Conversation focused)
    // ========================================================================
    /// j — focus next block
    FocusNextBlock,
    /// k — focus previous block
    FocusPrevBlock,
    /// Home / g→g — focus first block
    FocusFirstBlock,
    /// End / Shift+G — focus last block
    FocusLastBlock,
    /// f — expand focused block to full-screen reader
    ExpandBlock,
    /// Tab on thinking block — toggle collapse
    CollapseToggle,
    /// Esc in expanded view — pop back
    PopView,

    // ========================================================================
    // Scrolling
    // ========================================================================
    /// Mouse wheel or analog stick — scroll by pixel delta
    ScrollDelta(f32),
    /// Ctrl+U — half page up
    HalfPageUp,
    /// Ctrl+D — half page down
    HalfPageDown,
    /// Shift+G or End (in navigation) — scroll to end, enable follow
    ScrollToEnd,
    /// Home (in navigation) — scroll to top
    ScrollToTop,

    // ========================================================================
    // Tiling (Global, with Alt modifier)
    // ========================================================================
    /// Alt+H — focus pane to the left
    FocusPaneLeft,
    /// Alt+J — focus pane below
    FocusPaneDown,
    /// Alt+K — focus pane above
    FocusPaneUp,
    /// Alt+L — focus pane to the right
    FocusPaneRight,
    /// Alt+V — split pane vertically
    SplitVertical,
    /// Alt+S — split pane horizontally
    SplitHorizontal,
    /// Alt+Q — close focused pane
    ClosePane,
    /// Alt+] — grow focused pane (+5%)
    GrowPane,
    /// Alt+[ — shrink focused pane (-5%)
    ShrinkPane,
    /// Ctrl+^ — toggle between current and previous pane focus
    TogglePreviousPaneFocus,

    // ========================================================================
    // Constellation (constellation focused)
    // ========================================================================
    /// Backtick — toggle constellation overlay
    ToggleConstellation,
    /// hjkl spatial navigation between constellation nodes
    SpatialNav(Vec2),
    /// Shift+hjkl — pan the constellation camera
    Pan(Vec2),
    /// + or = — zoom in
    ZoomIn,
    /// - — zoom out
    ZoomOut,
    /// 0 — reset zoom to default
    ZoomReset,
    /// f in constellation — fork focused context
    ConstellationFork,
    /// m in constellation — open model picker
    ConstellationModelPicker,
    /// g→t — next context
    NextContext,
    /// g→T — previous context
    PrevContext,
    /// Ctrl+^ — toggle alternate context
    ToggleAlternate,

    // ========================================================================
    // Text editing (Compose or EditingBlock focused)
    // ========================================================================
    /// Enter in compose — submit text
    Submit,
    /// Backspace
    Backspace,
    /// Delete
    Delete,
    /// Arrow left
    CursorLeft,
    /// Arrow right
    CursorRight,
    /// Arrow up
    CursorUp,
    /// Arrow down
    CursorDown,
    /// Home — start of line
    CursorHome,
    /// End — end of line
    CursorEnd,
    /// Ctrl+Left — word left
    CursorWordLeft,
    /// Ctrl+Right — word right
    CursorWordRight,
    /// Ctrl+A — select all
    SelectAll,
    /// Ctrl+C — copy
    Copy,
    /// Ctrl+X — cut
    Cut,
    /// Ctrl+V — paste
    Paste,
    /// Ctrl+Z — undo
    Undo,
    /// Ctrl+Shift+Z — redo
    Redo,
    /// Shift+Enter — insert newline (without submitting)
    InsertNewline,

    // ========================================================================
    // Timeline (Navigation context)
    // ========================================================================
    /// [ — step back in history
    TimelineStepBack,
    /// ] — step forward in history
    TimelineStepForward,
    /// \ — jump to live/now
    TimelineJumpToLive,
    /// Ctrl+F (when historical) — fork from current timeline position
    TimelineFork,
    /// t — toggle timeline visibility
    TimelineToggle,

    // ========================================================================
    // App (Global)
    // ========================================================================
    /// q (in Navigation) or platform quit
    Quit,
    /// F12 — save screenshot
    Screenshot,
    /// F1 — toggle debug overlay
    DebugToggle,
}
