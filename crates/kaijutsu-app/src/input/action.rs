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
    /// Tab — cycle focus forward through Compose → Conversation → ...
    CycleFocusForward,
    /// Shift+Tab — cycle focus backward
    CycleFocusBackward,
    /// Shortcut to focus the compose area (i/Space in Navigation context)
    FocusCompose,
    /// Summon input overlay in chat mode (i/Space in Navigation)
    SummonChat,
    /// Toggle active surface between Chat and Shell (Ctrl+Z)
    ToggleSurface,
    /// Escape / gamepad East — the one "walk up a level" action
    /// (docs/input.md "Escape — two meanings total"):
    /// - Compose → Conversation (via double-Esc in Normal mode)
    /// - Dialog → cancel
    /// - well focus → overview → room; patch bay/station → room;
    ///   fsn → room; room → conversation
    PopLevel,
    /// Context-dependent "do the thing" (Enter)
    /// - Navigation: edit focused User Text block
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
    /// Tab on thinking block — toggle collapse
    CollapseToggle,

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
    /// Ctrl+V — paste the CLIPBOARD selection into compose (docs/input.md
    /// "xterm-style": Ctrl+C stays interrupt, copy rides selection).
    Paste,
    /// Middle-click — paste the PRIMARY selection (X11/Wayland), falling
    /// back to CLIPBOARD off-Linux.
    PastePrimary,
    /// Shift+Enter — insert newline (without submitting)
    InsertNewline,

    // ========================================================================
    // App (Global)
    // ========================================================================
    /// x (in Navigation) — toggle block excluded from conversation
    ToggleBlockExcluded,

    /// q (in Navigation) or platform quit
    Quit,
    /// F12 — save screenshot
    Screenshot,
    /// F1 — toggle debug overlay
    DebugToggle,

    // ========================================================================
    // Scene navigation (RoomNav / WellZoomed / PatchBayZoomed / StationZoomed)
    // ========================================================================
    /// Step forward within the current level: next carousel station, next
    /// ring seat (spinning it to the gate), next patch-bay wire.
    StepNext,
    /// Step backward within the current level.
    StepPrev,
    /// Move one detail level shallower (well: ring toward the mouth; at the
    /// mouth ring, rises into the hero pose).
    LevelUp,
    /// Move one detail level deeper (well: ring toward the throat; in the
    /// hero pose, returns to the mouth ring).
    LevelDown,
    /// Jump to seat *n* of the focused ring (well digits `0–9`).
    JumpSeat(usize),

    // Well verbs (fire-and-forget RPC on the selected context)
    /// `p` — take a ring-0 seat (unarchives if archived).
    Promote,
    /// `d` — one step outward on the kernel demote ladder.
    Demote,
    /// `c` — conclude the selected context.
    Conclude,
    /// `z` — toggle paused.
    PauseToggle,
    /// `a` — straight past the event horizon.
    Archive,

    /// `r` in the patch bay — rescan the ALSA graph.
    Rescan,
    /// `?` — toggle the in-scene keyboard legend.
    ToggleLegend,

    // FSN fly (continuous — emitted per frame while held / deflected)
    /// Camera-plane fly axis, -1..1 per component (WASD/arrows or left stick).
    FlyAxis { x: f32, y: f32 },
    /// Altitude axis, -1..1 (PgUp/PgDn, Equal/Minus).
    FlyAltitude(f32),

    // ========================================================================
    // Ctrl+A prefix verbs (input/prefix.rs; docs/input.md "The prefix table")
    // ========================================================================
    /// `Ctrl+A <digit>` — switch to ring-0 (ACTIVE rank) seat n, from anywhere.
    SwitchToActiveSeat(usize),
    /// `Ctrl+A Ctrl+A` — toggle to the MRU-previous context.
    SwitchToPreviousContext,
    /// `Ctrl+A n`/`p` — step to the next/previous ring-0 seat.
    ActiveSeatStep(i32),
    /// `Ctrl+A q` — demote the current context one ladder step and land on
    /// the MRU-previous one ("done for now"; the close half of screen's kill
    /// without the kill).
    CloseAndDemoteContext,
    /// `Ctrl+A w` / `Ctrl+A "` / gamepad Start — go to the well, from anywhere.
    GoToWell,
    /// `Ctrl+A d` — detach: back to the Conversation view from any scene or
    /// the editor (the editor session stays alive, suspend-style).
    DetachToConversation,
    /// `Ctrl+A a` — deliver a literal Ctrl+A to the focused vi surface.
    SendLiteralPrefix,
    /// `Ctrl+A A` — the prefilled-`kj` prompt: summon the shell with
    /// `kj context rename ` typed, cursor at end; Enter runs, Esc abandons.
    PromptContextRename,
    /// `Ctrl+A '` — same pattern, `kj context switch ` (switch-by-prompt).
    PromptContextSwitch,

    // ========================================================================
    // Context Interrupt (Ctrl+C in TextInput/Navigation)
    // ========================================================================
    /// Multi-press Ctrl+C — graduated cancel:
    /// - 1st press: soft interrupt (stop agentic loop after current tool turn)
    /// - 2nd press: hard interrupt (abort LLM stream + kill jobs)
    /// - 3rd press: hard interrupt + clear compose buffer
    ///
    /// `immediate` is the starting mode from the binding. The `handle_interrupt`
    /// system escalates based on press count.
    InterruptContext { immediate: bool },
}
