//! Default key bindings — the out-of-box binding table.
//!
//! Users (or Claude via BRP) can override individual bindings at runtime.
//! This function provides the starting configuration.

use bevy::prelude::*;

use super::action::Action;
use super::binding::{Binding, Modifiers};
use super::context::InputContext;
use bevy::input::gamepad::GamepadButton;

/// Build the default binding table for keyboard + gamepad input.
pub fn default_bindings() -> Vec<Binding> {
    let mut b = Vec::with_capacity(100);

    // ====================================================================
    // Global (always active, regardless of focus)
    // ====================================================================

    b.push(Binding::key(KeyCode::F1, InputContext::Global, Action::DebugToggle, "Toggle debug overlay"));
    b.push(Binding::key(KeyCode::F12, InputContext::Global, Action::Screenshot, "Save screenshot"));

    // Tiling: Alt+hjkl pane focus
    b.push(Binding::key_mod(KeyCode::KeyH, Modifiers::ALT, InputContext::Global, Action::FocusPaneLeft, "Focus pane left"));
    b.push(Binding::key_mod(KeyCode::KeyJ, Modifiers::ALT, InputContext::Global, Action::FocusPaneDown, "Focus pane below"));
    b.push(Binding::key_mod(KeyCode::KeyK, Modifiers::ALT, InputContext::Global, Action::FocusPaneUp, "Focus pane above"));
    b.push(Binding::key_mod(KeyCode::KeyL, Modifiers::ALT, InputContext::Global, Action::FocusPaneRight, "Focus pane right"));

    // Tiling: Alt+v/s split
    b.push(Binding::key_mod(KeyCode::KeyV, Modifiers::ALT, InputContext::Global, Action::SplitVertical, "Split pane vertical"));
    b.push(Binding::key_mod(KeyCode::KeyS, Modifiers::ALT, InputContext::Global, Action::SplitHorizontal, "Split pane horizontal"));

    // Tiling: Alt+q close, Alt+[/] resize
    b.push(Binding::key_mod(KeyCode::KeyQ, Modifiers::ALT, InputContext::Global, Action::ClosePane, "Close pane"));
    b.push(Binding::key_mod(KeyCode::BracketLeft, Modifiers::ALT, InputContext::Global, Action::ShrinkPane, "Shrink pane"));
    b.push(Binding::key_mod(KeyCode::BracketRight, Modifiers::ALT, InputContext::Global, Action::GrowPane, "Grow pane"));

    // Ctrl+^ (Ctrl+6) — toggle previous pane/alternate context
    b.push(Binding::key_mod(KeyCode::Digit6, Modifiers::CTRL, InputContext::Global, Action::ToggleAlternate, "Toggle alternate"));

    // ====================================================================
    // Navigation (conversation block list focused)
    // ====================================================================

    b.push(Binding::key(KeyCode::KeyJ, InputContext::Navigation, Action::FocusNextBlock, "Next block"));
    b.push(Binding::key(KeyCode::KeyK, InputContext::Navigation, Action::FocusPrevBlock, "Previous block"));
    b.push(Binding::key_mod(KeyCode::KeyG, Modifiers::SHIFT, InputContext::Navigation, Action::FocusLastBlock, "Last block"));
    b.push(Binding::key(KeyCode::Home, InputContext::Navigation, Action::FocusFirstBlock, "First block"));
    b.push(Binding::key(KeyCode::End, InputContext::Navigation, Action::FocusLastBlock, "Last block"));
    b.push(Binding::key(KeyCode::KeyF, InputContext::Navigation, Action::ExpandBlock, "Expand block"));
    b.push(Binding::key(KeyCode::Tab, InputContext::Navigation, Action::CycleFocusForward, "Cycle focus forward"));
    b.push(Binding::key_mod(KeyCode::Tab, Modifiers::SHIFT, InputContext::Navigation, Action::CycleFocusBackward, "Cycle focus backward"));

    // Quick shortcuts to compose
    b.push(Binding::key(KeyCode::KeyI, InputContext::Navigation, Action::FocusCompose, "Focus compose"));
    b.push(Binding::key(KeyCode::Space, InputContext::Navigation, Action::FocusCompose, "Focus compose"));

    // Activate (Enter on focused block → edit, or generic "do the thing")
    b.push(Binding::key(KeyCode::Enter, InputContext::Navigation, Action::Activate, "Activate focused block"));

    // Escape — pop view or close constellation
    b.push(Binding::key(KeyCode::Escape, InputContext::Navigation, Action::Unfocus, "Pop view / close overlay"));

    // Collapse toggle (Tab on thinking block — currently reuses Tab, but dispatch priority
    // means CycleFocusForward fires first. CollapseToggle is bound to a dedicated key
    // if needed, or handled by the Activate action on thinking blocks.)
    // For now: 'c' to collapse/toggle (mnemonic: collapse)
    b.push(Binding::key(KeyCode::KeyC, InputContext::Navigation, Action::CollapseToggle, "Toggle collapse"));

    // Quit (bare 'q' in navigation)
    b.push(Binding::key(KeyCode::KeyQ, InputContext::Navigation, Action::Quit, "Quit"));

    // Scroll
    b.push(Binding::key_mod(KeyCode::KeyD, Modifiers::CTRL, InputContext::Navigation, Action::HalfPageDown, "Half page down"));
    b.push(Binding::key_mod(KeyCode::KeyU, Modifiers::CTRL, InputContext::Navigation, Action::HalfPageUp, "Half page up"));

    // Backtick toggles constellation
    b.push(Binding::key(KeyCode::Backquote, InputContext::Navigation, Action::ToggleConstellation, "Toggle constellation"));

    // Timeline navigation
    b.push(Binding::key(KeyCode::BracketLeft, InputContext::Navigation, Action::TimelineStepBack, "Timeline step back"));
    b.push(Binding::key(KeyCode::BracketRight, InputContext::Navigation, Action::TimelineStepForward, "Timeline step forward"));
    b.push(Binding::key(KeyCode::Backslash, InputContext::Navigation, Action::TimelineJumpToLive, "Jump to live"));
    b.push(Binding::key_mod(KeyCode::KeyF, Modifiers::CTRL, InputContext::Navigation, Action::TimelineFork, "Fork from timeline"));
    b.push(Binding::key(KeyCode::KeyT, InputContext::Navigation, Action::TimelineToggle, "Toggle timeline"));

    // Sequences: g→t, g→T (Shift+T), g→g
    b.push(Binding::key_seq(KeyCode::KeyG, KeyCode::KeyT, InputContext::Navigation, Action::NextContext, "Next context"));
    b.push(Binding::key_seq_mod(
        KeyCode::KeyG,
        KeyCode::KeyT,
        Modifiers::SHIFT,
        InputContext::Navigation,
        Action::PrevContext,
        "Previous context",
    ));
    b.push(Binding::key_seq(KeyCode::KeyG, KeyCode::KeyG, InputContext::Navigation, Action::FocusFirstBlock, "First block"));

    // ====================================================================
    // Constellation (constellation node graph focused)
    // ====================================================================

    // Spatial nav: hjkl
    b.push(Binding::key(KeyCode::KeyH, InputContext::Constellation, Action::SpatialNav(Vec2::new(-1.0, 0.0)), "Navigate left"));
    b.push(Binding::key(KeyCode::KeyJ, InputContext::Constellation, Action::SpatialNav(Vec2::new(0.0, 1.0)), "Navigate down"));
    b.push(Binding::key(KeyCode::KeyK, InputContext::Constellation, Action::SpatialNav(Vec2::new(0.0, -1.0)), "Navigate up"));
    b.push(Binding::key(KeyCode::KeyL, InputContext::Constellation, Action::SpatialNav(Vec2::new(1.0, 0.0)), "Navigate right"));

    // Pan: Shift+hjkl
    b.push(Binding::key_mod(KeyCode::KeyH, Modifiers::SHIFT, InputContext::Constellation, Action::Pan(Vec2::new(-1.0, 0.0)), "Pan left"));
    b.push(Binding::key_mod(KeyCode::KeyJ, Modifiers::SHIFT, InputContext::Constellation, Action::Pan(Vec2::new(0.0, 1.0)), "Pan down"));
    b.push(Binding::key_mod(KeyCode::KeyK, Modifiers::SHIFT, InputContext::Constellation, Action::Pan(Vec2::new(0.0, -1.0)), "Pan up"));
    b.push(Binding::key_mod(KeyCode::KeyL, Modifiers::SHIFT, InputContext::Constellation, Action::Pan(Vec2::new(1.0, 0.0)), "Pan right"));

    // Zoom
    b.push(Binding::key_mod(KeyCode::Equal, Modifiers::SHIFT, InputContext::Constellation, Action::ZoomIn, "Zoom in"));
    b.push(Binding::key(KeyCode::Equal, InputContext::Constellation, Action::ZoomIn, "Zoom in"));
    b.push(Binding::key(KeyCode::Minus, InputContext::Constellation, Action::ZoomOut, "Zoom out"));
    b.push(Binding::key(KeyCode::Digit0, InputContext::Constellation, Action::ZoomReset, "Reset zoom"));

    // Actions
    b.push(Binding::key(KeyCode::Enter, InputContext::Constellation, Action::Activate, "Switch to context"));
    b.push(Binding::key(KeyCode::KeyF, InputContext::Constellation, Action::ConstellationFork, "Fork context"));
    b.push(Binding::key(KeyCode::KeyM, InputContext::Constellation, Action::ConstellationModelPicker, "Model picker"));
    b.push(Binding::key(KeyCode::Tab, InputContext::Constellation, Action::CycleFocusForward, "Cycle focus"));
    b.push(Binding::key(KeyCode::Escape, InputContext::Constellation, Action::Unfocus, "Close constellation"));

    // Sequences in constellation
    b.push(Binding::key_seq(KeyCode::KeyG, KeyCode::KeyT, InputContext::Constellation, Action::NextContext, "Next context"));
    b.push(Binding::key_seq_mod(
        KeyCode::KeyG,
        KeyCode::KeyT,
        Modifiers::SHIFT,
        InputContext::Constellation,
        Action::PrevContext,
        "Previous context",
    ));

    // ====================================================================
    // TextInput (compose area or block editing focused)
    // ====================================================================

    b.push(Binding::key(KeyCode::Enter, InputContext::TextInput, Action::Submit, "Submit"));
    b.push(Binding::key_mod(KeyCode::Enter, Modifiers::SHIFT, InputContext::TextInput, Action::InsertNewline, "Insert newline"));
    b.push(Binding::key(KeyCode::Escape, InputContext::TextInput, Action::Unfocus, "Return to navigation"));
    b.push(Binding::key(KeyCode::Tab, InputContext::TextInput, Action::CycleFocusForward, "Cycle focus"));
    b.push(Binding::key(KeyCode::Backspace, InputContext::TextInput, Action::Backspace, "Backspace"));
    b.push(Binding::key(KeyCode::Delete, InputContext::TextInput, Action::Delete, "Delete"));

    // Cursor movement
    b.push(Binding::key(KeyCode::ArrowLeft, InputContext::TextInput, Action::CursorLeft, "Cursor left"));
    b.push(Binding::key(KeyCode::ArrowRight, InputContext::TextInput, Action::CursorRight, "Cursor right"));
    b.push(Binding::key(KeyCode::ArrowUp, InputContext::TextInput, Action::CursorUp, "Cursor up"));
    b.push(Binding::key(KeyCode::ArrowDown, InputContext::TextInput, Action::CursorDown, "Cursor down"));
    b.push(Binding::key(KeyCode::Home, InputContext::TextInput, Action::CursorHome, "Start of line"));
    b.push(Binding::key(KeyCode::End, InputContext::TextInput, Action::CursorEnd, "End of line"));

    // Word movement
    b.push(Binding::key_mod(KeyCode::ArrowLeft, Modifiers::CTRL, InputContext::TextInput, Action::CursorWordLeft, "Word left"));
    b.push(Binding::key_mod(KeyCode::ArrowRight, Modifiers::CTRL, InputContext::TextInput, Action::CursorWordRight, "Word right"));

    // Clipboard + undo
    b.push(Binding::key_mod(KeyCode::KeyA, Modifiers::CTRL, InputContext::TextInput, Action::SelectAll, "Select all"));
    b.push(Binding::key_mod(KeyCode::KeyC, Modifiers::CTRL, InputContext::TextInput, Action::Copy, "Copy"));
    b.push(Binding::key_mod(KeyCode::KeyX, Modifiers::CTRL, InputContext::TextInput, Action::Cut, "Cut"));
    b.push(Binding::key_mod(KeyCode::KeyV, Modifiers::CTRL, InputContext::TextInput, Action::Paste, "Paste"));
    b.push(Binding::key_mod(KeyCode::KeyZ, Modifiers::CTRL, InputContext::TextInput, Action::Undo, "Undo"));
    b.push(Binding::key_mod(KeyCode::KeyZ, Modifiers::CTRL_SHIFT, InputContext::TextInput, Action::Redo, "Redo"));

    // ====================================================================
    // Dialog (modal dialog open)
    // ====================================================================

    b.push(Binding::key(KeyCode::Escape, InputContext::Dialog, Action::Unfocus, "Cancel dialog"));
    b.push(Binding::key(KeyCode::Enter, InputContext::Dialog, Action::Activate, "Confirm dialog"));
    b.push(Binding::key(KeyCode::KeyJ, InputContext::Dialog, Action::FocusNextBlock, "Next item"));
    b.push(Binding::key(KeyCode::KeyK, InputContext::Dialog, Action::FocusPrevBlock, "Previous item"));
    b.push(Binding::key(KeyCode::ArrowDown, InputContext::Dialog, Action::FocusNextBlock, "Next item"));
    b.push(Binding::key(KeyCode::ArrowUp, InputContext::Dialog, Action::FocusPrevBlock, "Previous item"));
    b.push(Binding::key(KeyCode::Backspace, InputContext::Dialog, Action::Backspace, "Backspace"));

    // ====================================================================
    // Dashboard
    // ====================================================================

    b.push(Binding::key(KeyCode::Enter, InputContext::Dashboard, Action::Activate, "Select"));

    // ====================================================================
    // Gamepad bindings
    // ====================================================================

    // South (A/X) — context-dependent activate
    b.push(Binding::gamepad(GamepadButton::South, InputContext::Navigation, Action::Activate, "Activate"));
    b.push(Binding::gamepad(GamepadButton::South, InputContext::Constellation, Action::Activate, "Switch to context"));
    b.push(Binding::gamepad(GamepadButton::South, InputContext::Dialog, Action::Activate, "Confirm"));
    b.push(Binding::gamepad(GamepadButton::South, InputContext::Dashboard, Action::Activate, "Select"));

    // East (B/O) — go back / cancel
    b.push(Binding::gamepad(GamepadButton::East, InputContext::Global, Action::Unfocus, "Back / Cancel"));

    // DPad — block navigation + constellation spatial nav
    b.push(Binding::gamepad(GamepadButton::DPadUp, InputContext::Navigation, Action::FocusPrevBlock, "Previous block"));
    b.push(Binding::gamepad(GamepadButton::DPadDown, InputContext::Navigation, Action::FocusNextBlock, "Next block"));
    b.push(Binding::gamepad(GamepadButton::DPadLeft, InputContext::Constellation, Action::SpatialNav(Vec2::new(-1.0, 0.0)), "Navigate left"));
    b.push(Binding::gamepad(GamepadButton::DPadRight, InputContext::Constellation, Action::SpatialNav(Vec2::new(1.0, 0.0)), "Navigate right"));
    b.push(Binding::gamepad(GamepadButton::DPadUp, InputContext::Constellation, Action::SpatialNav(Vec2::new(0.0, -1.0)), "Navigate up"));
    b.push(Binding::gamepad(GamepadButton::DPadDown, InputContext::Constellation, Action::SpatialNav(Vec2::new(0.0, 1.0)), "Navigate down"));
    b.push(Binding::gamepad(GamepadButton::DPadUp, InputContext::Dialog, Action::FocusPrevBlock, "Previous item"));
    b.push(Binding::gamepad(GamepadButton::DPadDown, InputContext::Dialog, Action::FocusNextBlock, "Next item"));

    // Triggers — page scroll
    b.push(Binding::gamepad(GamepadButton::LeftTrigger, InputContext::Navigation, Action::HalfPageUp, "Page up"));
    b.push(Binding::gamepad(GamepadButton::RightTrigger, InputContext::Navigation, Action::HalfPageDown, "Page down"));

    // Start — toggle constellation
    b.push(Binding::gamepad(GamepadButton::Start, InputContext::Global, Action::ToggleConstellation, "Toggle constellation"));

    // North (Y/△) — cycle focus
    b.push(Binding::gamepad(GamepadButton::North, InputContext::Navigation, Action::CycleFocusForward, "Cycle focus"));

    // West (X/□) — expand block
    b.push(Binding::gamepad(GamepadButton::West, InputContext::Navigation, Action::ExpandBlock, "Expand block"));

    b
}
