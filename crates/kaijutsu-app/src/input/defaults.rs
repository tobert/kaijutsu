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

    // Ctrl+^ (Ctrl+6) — toggle between current and previous pane focus
    b.push(Binding::key_mod(
        KeyCode::Digit6,
        Modifiers::CTRL,
        InputContext::Global,
        Action::TogglePreviousPaneFocus,
        "Toggle previous pane focus",
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

    // Escape — pop (Compose→Conversation, etc.)
    b.push(Binding::key(
        KeyCode::Escape,
        InputContext::Navigation,
        Action::PopLevel,
        "Pop view",
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
        Action::PopLevel,
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
    // RoomNav (Screen::Room, not zoomed — the octagon station carousel)
    // ====================================================================

    b.push(Binding::key(
        KeyCode::ArrowRight,
        InputContext::RoomNav,
        Action::StepNext,
        "Next station",
    ));
    b.push(Binding::key(
        KeyCode::Tab,
        InputContext::RoomNav,
        Action::StepNext,
        "Next station",
    ));
    b.push(Binding::key(
        KeyCode::ArrowLeft,
        InputContext::RoomNav,
        Action::StepPrev,
        "Previous station",
    ));
    b.push(Binding::key(
        KeyCode::Enter,
        InputContext::RoomNav,
        Action::Activate,
        "Dive into station",
    ));
    b.push(Binding::key(
        KeyCode::ArrowDown,
        InputContext::RoomNav,
        Action::Activate,
        "Dive into station",
    ));
    b.push(Binding::key(
        KeyCode::Escape,
        InputContext::RoomNav,
        Action::PopLevel,
        "Back to conversation",
    ));

    // ====================================================================
    // WellZoomed (Screen::Room, zoomed into the time well)
    // ====================================================================

    b.push(Binding::key(
        KeyCode::ArrowRight,
        InputContext::WellZoomed,
        Action::StepNext,
        "Spin ring forward",
    ));
    b.push(Binding::key(
        KeyCode::Tab,
        InputContext::WellZoomed,
        Action::StepNext,
        "Spin ring forward",
    ));
    b.push(Binding::key(
        KeyCode::ArrowLeft,
        InputContext::WellZoomed,
        Action::StepPrev,
        "Spin ring backward",
    ));
    b.push(Binding::key(
        KeyCode::ArrowUp,
        InputContext::WellZoomed,
        Action::LevelUp,
        "Shallower ring / hero pose",
    ));
    b.push(Binding::key(
        KeyCode::ArrowDown,
        InputContext::WellZoomed,
        Action::LevelDown,
        "Deeper ring",
    ));
    b.push(Binding::key(
        KeyCode::Enter,
        InputContext::WellZoomed,
        Action::Activate,
        "Focus / commit",
    ));
    b.push(Binding::key(
        KeyCode::Escape,
        InputContext::WellZoomed,
        Action::PopLevel,
        "Back out / leave well",
    ));
    // Digits 0-9 → jump to seat n of the focused ring
    for (i, key) in [
        KeyCode::Digit0,
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
    ]
    .into_iter()
    .enumerate()
    {
        b.push(Binding::key(
            key,
            InputContext::WellZoomed,
            Action::JumpSeat(i),
            format!("Jump to seat {i}"),
        ));
    }
    // Placement verbs (the in-well legend renders these from this table)
    b.push(Binding::key(
        KeyCode::KeyP,
        InputContext::WellZoomed,
        Action::Promote,
        "Promote to active ring",
    ));
    b.push(Binding::key(
        KeyCode::KeyD,
        InputContext::WellZoomed,
        Action::Demote,
        "Demote one step outward",
    ));
    b.push(Binding::key(
        KeyCode::KeyC,
        InputContext::WellZoomed,
        Action::Conclude,
        "Conclude context",
    ));
    b.push(Binding::key(
        KeyCode::KeyZ,
        InputContext::WellZoomed,
        Action::PauseToggle,
        "Toggle paused",
    ));
    b.push(Binding::key(
        KeyCode::KeyA,
        InputContext::WellZoomed,
        Action::Archive,
        "Archive past the horizon",
    ));
    // `?` legend — bind both bare Slash and Shift+Slash so the shifted
    // glyph on every layout that puts ? over / keeps working.
    b.push(Binding::key(
        KeyCode::Slash,
        InputContext::WellZoomed,
        Action::ToggleLegend,
        "Toggle legend",
    ));
    b.push(Binding::key_mod(
        KeyCode::Slash,
        Modifiers::SHIFT,
        InputContext::WellZoomed,
        Action::ToggleLegend,
        "Toggle legend",
    ));

    // ====================================================================
    // PatchBayZoomed (Screen::Room, zoomed into the patch bay)
    // ====================================================================

    b.push(Binding::key(
        KeyCode::ArrowRight,
        InputContext::PatchBayZoomed,
        Action::StepNext,
        "Next wire",
    ));
    b.push(Binding::key(
        KeyCode::Tab,
        InputContext::PatchBayZoomed,
        Action::StepNext,
        "Next wire",
    ));
    b.push(Binding::key(
        KeyCode::ArrowLeft,
        InputContext::PatchBayZoomed,
        Action::StepPrev,
        "Previous wire",
    ));
    b.push(Binding::key(
        KeyCode::KeyR,
        InputContext::PatchBayZoomed,
        Action::Rescan,
        "Rescan ALSA graph",
    ));
    b.push(Binding::key(
        KeyCode::ArrowUp,
        InputContext::PatchBayZoomed,
        Action::PopLevel,
        "Back to room",
    ));
    b.push(Binding::key(
        KeyCode::Escape,
        InputContext::PatchBayZoomed,
        Action::PopLevel,
        "Back to room",
    ));

    // ====================================================================
    // StationZoomed (zoomed station with no keyboard of its own)
    // ====================================================================

    b.push(Binding::key(
        KeyCode::ArrowUp,
        InputContext::StationZoomed,
        Action::PopLevel,
        "Back to room",
    ));
    b.push(Binding::key(
        KeyCode::Escape,
        InputContext::StationZoomed,
        Action::PopLevel,
        "Back to room",
    ));

    // ====================================================================
    // FsnFly (Screen::Fsn — fly keys are polled continuously in dispatch)
    // ====================================================================

    b.push(Binding::key(
        KeyCode::Escape,
        InputContext::FsnFly,
        Action::PopLevel,
        "Back to room",
    ));

    // ====================================================================
    // Gamepad bindings
    // ====================================================================

    // Scene navigation — dpad steps, South dives/commits (East pops via
    // the Global binding below).
    for ctx in [
        InputContext::RoomNav,
        InputContext::WellZoomed,
        InputContext::PatchBayZoomed,
    ] {
        b.push(Binding::gamepad(
            GamepadButton::DPadRight,
            ctx,
            Action::StepNext,
            "Step forward",
        ));
        b.push(Binding::gamepad(
            GamepadButton::DPadLeft,
            ctx,
            Action::StepPrev,
            "Step backward",
        ));
    }
    b.push(Binding::gamepad(
        GamepadButton::South,
        InputContext::RoomNav,
        Action::Activate,
        "Dive into station",
    ));
    b.push(Binding::gamepad(
        GamepadButton::South,
        InputContext::WellZoomed,
        Action::Activate,
        "Focus / commit",
    ));
    b.push(Binding::gamepad(
        GamepadButton::DPadUp,
        InputContext::WellZoomed,
        Action::LevelUp,
        "Shallower ring",
    ));
    b.push(Binding::gamepad(
        GamepadButton::DPadDown,
        InputContext::WellZoomed,
        Action::LevelDown,
        "Deeper ring",
    ));
    b.push(Binding::gamepad(
        GamepadButton::DPadDown,
        InputContext::RoomNav,
        Action::Activate,
        "Dive into station",
    ));

    // South (A/X) — context-dependent activate
    b.push(Binding::gamepad(
        GamepadButton::South,
        InputContext::Navigation,
        Action::Activate,
        "Activate",
    ));
    b.push(Binding::gamepad(
        GamepadButton::South,
        InputContext::Dialog,
        Action::Activate,
        "Confirm",
    ));

    // East (B/O) — go back / cancel / pop one level, everywhere
    b.push(Binding::gamepad(
        GamepadButton::East,
        InputContext::Global,
        Action::PopLevel,
        "Back / pop level",
    ));

    // Start (|||) — go to the well, from anywhere: the pad-side Ctrl+A w.
    b.push(Binding::gamepad(
        GamepadButton::Start,
        InputContext::Global,
        Action::GoToWell,
        "Go to the well",
    ));

    // DPad — block navigation
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

    // North (Y/△) — cycle focus
    b.push(Binding::gamepad(
        GamepadButton::North,
        InputContext::Navigation,
        Action::CycleFocusForward,
        "Cycle focus",
    ));

    b
}
