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

    b.push(Binding::key(
        KeyCode::F1,
        InputContext::Global,
        Action::DebugToggle,
        "Toggle debug overlay",
    ));
    b.push(Binding::key(
        KeyCode::F12,
        InputContext::Global,
        Action::Screenshot,
        "Save screenshot",
    ));

    // Tiling: Alt+hjkl pane focus
    b.push(Binding::key_mod(
        KeyCode::KeyH,
        Modifiers::ALT,
        InputContext::Global,
        Action::FocusPaneLeft,
        "Focus pane left",
    ));
    b.push(Binding::key_mod(
        KeyCode::KeyJ,
        Modifiers::ALT,
        InputContext::Global,
        Action::FocusPaneDown,
        "Focus pane below",
    ));
    b.push(Binding::key_mod(
        KeyCode::KeyK,
        Modifiers::ALT,
        InputContext::Global,
        Action::FocusPaneUp,
        "Focus pane above",
    ));
    b.push(Binding::key_mod(
        KeyCode::KeyL,
        Modifiers::ALT,
        InputContext::Global,
        Action::FocusPaneRight,
        "Focus pane right",
    ));

    // Tiling: Alt+v/s split
    b.push(Binding::key_mod(
        KeyCode::KeyV,
        Modifiers::ALT,
        InputContext::Global,
        Action::SplitVertical,
        "Split pane vertical",
    ));
    b.push(Binding::key_mod(
        KeyCode::KeyS,
        Modifiers::ALT,
        InputContext::Global,
        Action::SplitHorizontal,
        "Split pane horizontal",
    ));

    // Tiling: Alt+q close, Alt+[/] resize
    b.push(Binding::key_mod(
        KeyCode::KeyQ,
        Modifiers::ALT,
        InputContext::Global,
        Action::ClosePane,
        "Close pane",
    ));
    b.push(Binding::key_mod(
        KeyCode::BracketLeft,
        Modifiers::ALT,
        InputContext::Global,
        Action::ShrinkPane,
        "Shrink pane",
    ));
    b.push(Binding::key_mod(
        KeyCode::BracketRight,
        Modifiers::ALT,
        InputContext::Global,
        Action::GrowPane,
        "Grow pane",
    ));

    // Ctrl+^ (Ctrl+6) — toggle previous pane/alternate context
    b.push(Binding::key_mod(
        KeyCode::Digit6,
        Modifiers::CTRL,
        InputContext::Global,
        Action::ToggleAlternate,
        "Toggle alternate",
    ));

    // ====================================================================
    // Navigation (conversation block list focused)
    // ====================================================================

    b.push(Binding::key(
        KeyCode::KeyJ,
        InputContext::Navigation,
        Action::FocusNextBlock,
        "Next block",
    ));
    b.push(Binding::key(
        KeyCode::KeyK,
        InputContext::Navigation,
        Action::FocusPrevBlock,
        "Previous block",
    ));
    b.push(Binding::key_mod(
        KeyCode::KeyG,
        Modifiers::SHIFT,
        InputContext::Navigation,
        Action::FocusLastBlock,
        "Last block",
    ));
    b.push(Binding::key(
        KeyCode::Home,
        InputContext::Navigation,
        Action::FocusFirstBlock,
        "First block",
    ));
    b.push(Binding::key(
        KeyCode::End,
        InputContext::Navigation,
        Action::FocusLastBlock,
        "Last block",
    ));
    b.push(Binding::key(
        KeyCode::KeyF,
        InputContext::Navigation,
        Action::ExpandBlock,
        "Expand block",
    ));
    b.push(Binding::key(
        KeyCode::KeyX,
        InputContext::Navigation,
        Action::ToggleBlockExcluded,
        "Toggle block excluded",
    ));
    b.push(Binding::key(
        KeyCode::Tab,
        InputContext::Navigation,
        Action::CycleFocusForward,
        "Cycle focus forward",
    ));
    b.push(Binding::key_mod(
        KeyCode::Tab,
        Modifiers::SHIFT,
        InputContext::Navigation,
        Action::CycleFocusBackward,
        "Cycle focus backward",
    ));

    // Summon input overlay
    b.push(Binding::key(
        KeyCode::KeyI,
        InputContext::Navigation,
        Action::SummonChat,
        "Summon chat input",
    ));
    b.push(Binding::key(
        KeyCode::Space,
        InputContext::Navigation,
        Action::SummonChat,
        "Summon chat input",
    ));
    b.push(Binding::key_mod(
        KeyCode::KeyZ,
        Modifiers::CTRL,
        InputContext::Navigation,
        Action::ToggleSurface,
        "Toggle shell/chat surface",
    ));

    // Activate (Enter on focused block → edit, or generic "do the thing")
    b.push(Binding::key(
        KeyCode::Enter,
        InputContext::Navigation,
        Action::Activate,
        "Activate focused block",
    ));

    // Escape — unfocus (Compose→Conversation, etc.)
    b.push(Binding::key(
        KeyCode::Escape,
        InputContext::Navigation,
        Action::Unfocus,
        "Unfocus / pop view",
    ));

    // Ctrl+C — multi-press interrupt (soft → hard → hard+clear)
    b.push(Binding::key_mod(
        KeyCode::KeyC,
        Modifiers::CTRL,
        InputContext::Navigation,
        Action::InterruptContext { immediate: false },
        "Interrupt context",
    ));

    // Collapse toggle (Tab on thinking block — currently reuses Tab, but dispatch priority
    // means CycleFocusForward fires first. CollapseToggle is bound to a dedicated key
    // if needed, or handled by the Activate action on thinking blocks.)
    // For now: 'c' to collapse/toggle (mnemonic: collapse)
    b.push(Binding::key(
        KeyCode::KeyC,
        InputContext::Navigation,
        Action::CollapseToggle,
        "Toggle collapse",
    ));

    // Quit (bare 'q' in navigation)
    b.push(Binding::key(
        KeyCode::KeyQ,
        InputContext::Navigation,
        Action::Quit,
        "Quit",
    ));

    // Scroll
    b.push(Binding::key_mod(
        KeyCode::KeyD,
        Modifiers::CTRL,
        InputContext::Navigation,
        Action::HalfPageDown,
        "Half page down",
    ));
    b.push(Binding::key_mod(
        KeyCode::KeyU,
        Modifiers::CTRL,
        InputContext::Navigation,
        Action::HalfPageUp,
        "Half page up",
    ));
    b.push(Binding::key_mod(
        KeyCode::Home,
        Modifiers::CTRL,
        InputContext::Navigation,
        Action::ScrollToTop,
        "Scroll to top",
    ));
    b.push(Binding::key_mod(
        KeyCode::End,
        Modifiers::CTRL,
        InputContext::Navigation,
        Action::ScrollToEnd,
        "Scroll to end",
    ));

    // Backtick toggles constellation
    b.push(Binding::key(
        KeyCode::Backquote,
        InputContext::Navigation,
        Action::ToggleConstellation,
        "Toggle constellation",
    ));

    // ====================================================================
    // Constellation (force-directed graph focused)
    // ====================================================================

    // Spatial navigation (hjkl)
    b.push(Binding::key(
        KeyCode::KeyH,
        InputContext::Constellation,
        Action::SpatialNav(Vec2::new(-1.0, 0.0)),
        "Navigate left",
    ));
    b.push(Binding::key(
        KeyCode::KeyL,
        InputContext::Constellation,
        Action::SpatialNav(Vec2::new(1.0, 0.0)),
        "Navigate right",
    ));
    b.push(Binding::key(
        KeyCode::KeyK,
        InputContext::Constellation,
        Action::SpatialNav(Vec2::new(0.0, -1.0)),
        "Navigate up",
    ));
    b.push(Binding::key(
        KeyCode::KeyJ,
        InputContext::Constellation,
        Action::SpatialNav(Vec2::new(0.0, 1.0)),
        "Navigate down",
    ));

    // Actions
    b.push(Binding::key(
        KeyCode::Enter,
        InputContext::Constellation,
        Action::Activate,
        "Switch to context",
    ));
    b.push(Binding::key(
        KeyCode::KeyN,
        InputContext::Constellation,
        Action::ConstellationCreate,
        "New context",
    ));
    b.push(Binding::key(
        KeyCode::KeyM,
        InputContext::Constellation,
        Action::ConstellationModelPicker,
        "Model picker",
    ));
    b.push(Binding::key(
        KeyCode::KeyA,
        InputContext::Constellation,
        Action::ConstellationArchive,
        "Archive context",
    ));
    b.push(Binding::key(
        KeyCode::Tab,
        InputContext::Constellation,
        Action::CycleFocusForward,
        "Cycle focus",
    ));
    b.push(Binding::key(
        KeyCode::Escape,
        InputContext::Constellation,
        Action::Unfocus,
        "Close constellation",
    ));

    // ====================================================================
    // TextInput (compose area — owned by VimMachine)
    // ====================================================================
    // All TextInput keyboard bindings are handled by the VimMachine
    // (vim_dispatch_compose system). Ctrl+C for interrupt is handled
    // directly in that system before keys reach the VimMachine.
    //
    // No flat bindings needed here — the VimMachine handles:
    //   Insert mode: character typing, Backspace, Delete, arrow keys
    //   Normal mode: motions (hjkl, w/b/e, etc.), operators (d, c, y)
    //   Submit: Enter (via submit_on_enter)
    //   Mode ring: Tab (via custom binding, Phase 2+)
    //   Escape: Normal mode ↔ Insert mode, or dismiss compose

    // ====================================================================
    // Dialog (modal dialog open)
    // ====================================================================

    b.push(Binding::key(
        KeyCode::Escape,
        InputContext::Dialog,
        Action::Unfocus,
        "Cancel dialog",
    ));
    b.push(Binding::key(
        KeyCode::Enter,
        InputContext::Dialog,
        Action::Activate,
        "Confirm dialog",
    ));
    // Tab cycles form fields in dialogs (Dialog has priority 2 > TextInput priority 1, so this wins)
    b.push(Binding::key(
        KeyCode::Tab,
        InputContext::Dialog,
        Action::CycleFocusForward,
        "Cycle form field",
    ));
    b.push(Binding::key_mod(
        KeyCode::Tab,
        Modifiers::SHIFT,
        InputContext::Dialog,
        Action::CycleFocusBackward,
        "Cycle form field backward",
    ));
    b.push(Binding::key(
        KeyCode::KeyJ,
        InputContext::Dialog,
        Action::FocusNextBlock,
        "Next item",
    ));
    b.push(Binding::key(
        KeyCode::KeyK,
        InputContext::Dialog,
        Action::FocusPrevBlock,
        "Previous item",
    ));
    b.push(Binding::key(
        KeyCode::ArrowDown,
        InputContext::Dialog,
        Action::FocusNextBlock,
        "Next item",
    ));
    b.push(Binding::key(
        KeyCode::ArrowUp,
        InputContext::Dialog,
        Action::FocusPrevBlock,
        "Previous item",
    ));
    b.push(Binding::key(
        KeyCode::Backspace,
        InputContext::Dialog,
        Action::Backspace,
        "Backspace",
    ));

    // ====================================================================
    // Gamepad bindings
    // ====================================================================

    // South (A/X) — context-dependent activate
    b.push(Binding::gamepad(
        GamepadButton::South,
        InputContext::Navigation,
        Action::Activate,
        "Activate",
    ));
    b.push(Binding::gamepad(
        GamepadButton::South,
        InputContext::Constellation,
        Action::Activate,
        "Switch to context",
    ));
    b.push(Binding::gamepad(
        GamepadButton::South,
        InputContext::Dialog,
        Action::Activate,
        "Confirm",
    ));

    // East (B/O) — go back / cancel
    b.push(Binding::gamepad(
        GamepadButton::East,
        InputContext::Global,
        Action::Unfocus,
        "Back / Cancel",
    ));

    // DPad — block navigation + constellation spatial nav
    b.push(Binding::gamepad(
        GamepadButton::DPadUp,
        InputContext::Navigation,
        Action::FocusPrevBlock,
        "Previous block",
    ));
    b.push(Binding::gamepad(
        GamepadButton::DPadDown,
        InputContext::Navigation,
        Action::FocusNextBlock,
        "Next block",
    ));
    b.push(Binding::gamepad(
        GamepadButton::DPadLeft,
        InputContext::Constellation,
        Action::SpatialNav(Vec2::new(-1.0, 0.0)),
        "Navigate left",
    ));
    b.push(Binding::gamepad(
        GamepadButton::DPadRight,
        InputContext::Constellation,
        Action::SpatialNav(Vec2::new(1.0, 0.0)),
        "Navigate right",
    ));
    b.push(Binding::gamepad(
        GamepadButton::DPadUp,
        InputContext::Constellation,
        Action::SpatialNav(Vec2::new(0.0, -1.0)),
        "Navigate up",
    ));
    b.push(Binding::gamepad(
        GamepadButton::DPadDown,
        InputContext::Constellation,
        Action::SpatialNav(Vec2::new(0.0, 1.0)),
        "Navigate down",
    ));
    b.push(Binding::gamepad(
        GamepadButton::DPadUp,
        InputContext::Dialog,
        Action::FocusPrevBlock,
        "Previous item",
    ));
    b.push(Binding::gamepad(
        GamepadButton::DPadDown,
        InputContext::Dialog,
        Action::FocusNextBlock,
        "Next item",
    ));

    // Triggers — page scroll
    b.push(Binding::gamepad(
        GamepadButton::LeftTrigger,
        InputContext::Navigation,
        Action::HalfPageUp,
        "Page up",
    ));
    b.push(Binding::gamepad(
        GamepadButton::RightTrigger,
        InputContext::Navigation,
        Action::HalfPageDown,
        "Page down",
    ));

    // Start — toggle constellation
    b.push(Binding::gamepad(
        GamepadButton::Start,
        InputContext::Global,
        Action::ToggleConstellation,
        "Toggle constellation",
    ));

    // North (Y/△) — cycle focus
    b.push(Binding::gamepad(
        GamepadButton::North,
        InputContext::Navigation,
        Action::CycleFocusForward,
        "Cycle focus",
    ));

    // West (X/□) — expand block
    b.push(Binding::gamepad(
        GamepadButton::West,
        InputContext::Navigation,
        Action::ExpandBlock,
        "Expand block",
    ));

    b
}
