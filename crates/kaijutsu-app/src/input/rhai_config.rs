//! Bindings configuration for Kaijutsu.
//!
//! Loads key bindings from `~/.config/kaijutsu/bindings.toml` at startup.
//! Falls back to `default_bindings()` if the file doesn't exist or has errors.
//!
//! ## TOML format
//!
//! ```toml
//! [[bindings]]
//! key = "KeyJ"
//! context = "Navigation"
//! action = "FocusNextBlock"
//! label = "Next block"
//!
//! [[bindings]]
//! key = "KeyD"
//! modifiers = "CTRL"
//! context = "Navigation"
//! action = "HalfPageDown"
//! label = "Half page down"
//! ```

use bevy::input::gamepad::GamepadButton;
use bevy::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::action::Action;
use super::binding::{Binding, Modifiers};
use super::context::InputContext;
use super::defaults::default_bindings;

// ============================================================================
// TOML binding entry
// ============================================================================

/// A single binding entry as it appears in bindings.toml.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindingEntry {
    pub key: String,
    #[serde(default)]
    pub modifiers: String,
    pub context: String,
    pub action: String,
    #[serde(default)]
    pub gamepad: bool,
    #[serde(default)]
    pub label: String,
}

/// Top-level bindings.toml structure.
#[derive(Debug, Deserialize)]
struct BindingsToml {
    #[serde(default)]
    bindings: Vec<BindingEntry>,
}

// ============================================================================
// FILE PATH
// ============================================================================

/// Get the bindings config file path (~/.config/kaijutsu/bindings.toml).
pub fn bindings_file_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("kaijutsu").join("bindings.toml"))
}

// ============================================================================
// LOADING
// ============================================================================

/// Load bindings from the user's config file at startup.
///
/// Falls back to default bindings if the file doesn't exist or has errors.
pub fn load_bindings() -> Vec<Binding> {
    let Some(path) = bindings_file_path() else {
        info!("No config directory available, using default bindings");
        return default_bindings();
    };

    if !path.exists() {
        info!("Bindings not found at {:?}, using defaults", path);
        return default_bindings();
    }

    let content = match std::fs::read_to_string(&path) {
        Ok(s) => {
            info!("Loaded bindings from {:?}", path);
            s
        }
        Err(e) => {
            warn!("Failed to read bindings {:?}: {}", path, e);
            return default_bindings();
        }
    };

    match parse_bindings_toml(&content) {
        Ok(bindings) => bindings,
        Err(e) => {
            warn!("Failed to parse bindings: {}", e);
            warn!("Falling back to default bindings");
            default_bindings()
        }
    }
}

/// Parse a TOML string into a Vec<Binding>.
pub fn parse_bindings_toml(content: &str) -> Result<Vec<Binding>, String> {
    let parsed: BindingsToml =
        toml::from_str(content).map_err(|e| format!("TOML parse error: {e}"))?;

    let mut bindings = Vec::with_capacity(parsed.bindings.len());
    for entry in parsed.bindings {
        match binding_from_entry(&entry) {
            Ok(b) => bindings.push(b),
            Err(e) => warn!("Skipping invalid binding: {}", e),
        }
    }

    info!("Parsed {} bindings from TOML", bindings.len());
    Ok(bindings)
}

/// Serialize bindings to TOML format (for writing defaults or app-managed config).
pub fn bindings_to_toml(bindings: &[Binding]) -> String {
    let entries: Vec<BindingEntry> = bindings.iter().map(binding_to_entry).collect();
    let toml_struct = BindingsTomlOut { bindings: entries };
    toml::to_string_pretty(&toml_struct).unwrap_or_default()
}

#[derive(Serialize)]
struct BindingsTomlOut {
    bindings: Vec<BindingEntry>,
}

// ============================================================================
// SERIALIZATION: Binding ↔ BindingEntry
// ============================================================================

fn binding_to_entry(b: &Binding) -> BindingEntry {
    let (key_str, is_gamepad) = match &b.source {
        super::binding::InputSource::Key(k) => (format!("{:?}", k), false),
        super::binding::InputSource::GamepadButton(btn) => (format!("{:?}", btn), true),
    };
    BindingEntry {
        key: key_str,
        modifiers: modifiers_to_str(&b.modifiers),
        context: context_to_str(b.context),
        action: action_to_str(&b.action),
        gamepad: is_gamepad,
        label: b.description.clone(),
    }
}

fn binding_from_entry(e: &BindingEntry) -> Result<Binding, String> {
    let context = parse_context(&e.context)?;
    let action = parse_action(&e.action)?;
    let modifiers = parse_modifiers(&e.modifiers);

    let source = if e.gamepad {
        super::binding::InputSource::GamepadButton(parse_gamepad_button(&e.key)?)
    } else {
        super::binding::InputSource::Key(parse_key_code(&e.key)?)
    };

    Ok(Binding {
        source,
        modifiers,
        context,
        action,
        description: e.label.clone(),
    })
}

// ============================================================================
// STRING ↔ ENUM CONVERSIONS (Rhai-independent)
// ============================================================================

fn modifiers_to_str(m: &Modifiers) -> String {
    let mut parts = Vec::new();
    if m.ctrl {
        parts.push("CTRL");
    }
    if m.shift {
        parts.push("SHIFT");
    }
    if m.alt {
        parts.push("ALT");
    }
    if m.super_key {
        parts.push("SUPER");
    }
    parts.join("+")
}

fn parse_modifiers(s: &str) -> Modifiers {
    if s.is_empty() {
        return Modifiers::NONE;
    }
    let mut m = Modifiers::NONE;
    for part in s.split('+') {
        match part.trim() {
            "CTRL" => m.ctrl = true,
            "SHIFT" => m.shift = true,
            "ALT" => m.alt = true,
            "SUPER" => m.super_key = true,
            other => warn!("Unknown modifier '{other}' in bindings"),
        }
    }
    m
}

fn context_to_str(ctx: InputContext) -> String {
    match ctx {
        InputContext::Global => "Global",
        InputContext::Navigation => "Navigation",
        InputContext::TextInput => "TextInput",
        InputContext::Constellation => "Constellation",
        InputContext::Dialog => "Dialog",
    }
    .to_string()
}

fn parse_context(s: &str) -> Result<InputContext, String> {
    match s {
        "Global" => Ok(InputContext::Global),
        "Navigation" => Ok(InputContext::Navigation),
        "TextInput" => Ok(InputContext::TextInput),
        "Constellation" => Ok(InputContext::Constellation),
        "Dialog" => Ok(InputContext::Dialog),
        _ => Err(format!("unknown context '{s}'")),
    }
}

fn action_to_str(a: &Action) -> String {
    match a {
        Action::CycleFocusForward => "CycleFocusForward",
        Action::CycleFocusBackward => "CycleFocusBackward",
        Action::FocusCompose => "FocusCompose",
        Action::SummonChat => "SummonChat",
        Action::ToggleSurface => "ToggleSurface",
        Action::ToggleBlockExcluded => "ToggleBlockExcluded",
        Action::Unfocus => "Unfocus",
        Action::Activate => "Activate",
        Action::FocusNextBlock => "FocusNextBlock",
        Action::FocusPrevBlock => "FocusPrevBlock",
        Action::FocusFirstBlock => "FocusFirstBlock",
        Action::FocusLastBlock => "FocusLastBlock",
        Action::ExpandBlock => "ExpandBlock",
        Action::ToggleStackView => "ToggleStackView",
        Action::CollapseToggle => "CollapseToggle",
        Action::ScrollDelta(_) => "ScrollDelta",
        Action::HalfPageUp => "HalfPageUp",
        Action::HalfPageDown => "HalfPageDown",
        Action::ScrollToEnd => "ScrollToEnd",
        Action::ScrollToTop => "ScrollToTop",
        Action::FocusPaneLeft => "FocusPaneLeft",
        Action::FocusPaneDown => "FocusPaneDown",
        Action::FocusPaneUp => "FocusPaneUp",
        Action::FocusPaneRight => "FocusPaneRight",
        Action::SplitVertical => "SplitVertical",
        Action::SplitHorizontal => "SplitHorizontal",
        Action::ClosePane => "ClosePane",
        Action::GrowPane => "GrowPane",
        Action::ShrinkPane => "ShrinkPane",
        Action::TogglePreviousPaneFocus => "TogglePreviousPaneFocus",
        Action::ToggleConstellation => "ToggleConstellation",
        Action::SpatialNav(_) => "SpatialNav",
        Action::Pan(_) => "Pan",
        Action::ZoomIn => "ZoomIn",
        Action::ZoomOut => "ZoomOut",
        Action::ZoomReset => "ZoomReset",
        Action::ConstellationCreate => "ConstellationCreate",
        Action::ConstellationModelPicker => "ConstellationModelPicker",
        Action::ConstellationArchive => "ConstellationArchive",
        Action::ToggleAlternate => "ToggleAlternate",
        Action::Submit => "Submit",
        Action::Backspace => "Backspace",
        Action::Delete => "Delete",
        Action::CursorLeft => "CursorLeft",
        Action::CursorRight => "CursorRight",
        Action::CursorUp => "CursorUp",
        Action::CursorDown => "CursorDown",
        Action::CursorHome => "CursorHome",
        Action::CursorEnd => "CursorEnd",
        Action::CursorWordLeft => "CursorWordLeft",
        Action::CursorWordRight => "CursorWordRight",
        Action::SelectAll => "SelectAll",
        Action::Copy => "Copy",
        Action::Cut => "Cut",
        Action::Paste => "Paste",
        Action::Undo => "Undo",
        Action::Redo => "Redo",
        Action::InsertNewline => "InsertNewline",
        Action::Quit => "Quit",
        Action::Screenshot => "Screenshot",
        Action::DebugToggle => "DebugToggle",
        Action::InterruptContext { .. } => "InterruptContext",
    }
    .to_string()
}

fn parse_action(s: &str) -> Result<Action, String> {
    match s {
        "CycleFocusForward" => Ok(Action::CycleFocusForward),
        "CycleFocusBackward" => Ok(Action::CycleFocusBackward),
        "FocusCompose" => Ok(Action::FocusCompose),
        "SummonChat" => Ok(Action::SummonChat),
        "ToggleSurface" => Ok(Action::ToggleSurface),
        "ToggleBlockExcluded" => Ok(Action::ToggleBlockExcluded),
        "Unfocus" => Ok(Action::Unfocus),
        "Activate" => Ok(Action::Activate),
        "FocusNextBlock" => Ok(Action::FocusNextBlock),
        "FocusPrevBlock" => Ok(Action::FocusPrevBlock),
        "FocusFirstBlock" => Ok(Action::FocusFirstBlock),
        "FocusLastBlock" => Ok(Action::FocusLastBlock),
        "ExpandBlock" => Ok(Action::ExpandBlock),
        "ToggleStackView" => Ok(Action::ToggleStackView),
        "CollapseToggle" => Ok(Action::CollapseToggle),
        "HalfPageUp" => Ok(Action::HalfPageUp),
        "HalfPageDown" => Ok(Action::HalfPageDown),
        "ScrollToEnd" => Ok(Action::ScrollToEnd),
        "ScrollToTop" => Ok(Action::ScrollToTop),
        "FocusPaneLeft" => Ok(Action::FocusPaneLeft),
        "FocusPaneDown" => Ok(Action::FocusPaneDown),
        "FocusPaneUp" => Ok(Action::FocusPaneUp),
        "FocusPaneRight" => Ok(Action::FocusPaneRight),
        "SplitVertical" => Ok(Action::SplitVertical),
        "SplitHorizontal" => Ok(Action::SplitHorizontal),
        "ClosePane" => Ok(Action::ClosePane),
        "GrowPane" => Ok(Action::GrowPane),
        "ShrinkPane" => Ok(Action::ShrinkPane),
        "TogglePreviousPaneFocus" => Ok(Action::TogglePreviousPaneFocus),
        "ToggleConstellation" => Ok(Action::ToggleConstellation),
        "ZoomIn" => Ok(Action::ZoomIn),
        "ZoomOut" => Ok(Action::ZoomOut),
        "ZoomReset" => Ok(Action::ZoomReset),
        "ConstellationCreate" => Ok(Action::ConstellationCreate),
        "ConstellationModelPicker" => Ok(Action::ConstellationModelPicker),
        "ConstellationArchive" => Ok(Action::ConstellationArchive),
        "ToggleAlternate" => Ok(Action::ToggleAlternate),
        "Submit" => Ok(Action::Submit),
        "Backspace" => Ok(Action::Backspace),
        "Delete" => Ok(Action::Delete),
        "CursorLeft" => Ok(Action::CursorLeft),
        "CursorRight" => Ok(Action::CursorRight),
        "CursorUp" => Ok(Action::CursorUp),
        "CursorDown" => Ok(Action::CursorDown),
        "CursorHome" => Ok(Action::CursorHome),
        "CursorEnd" => Ok(Action::CursorEnd),
        "CursorWordLeft" => Ok(Action::CursorWordLeft),
        "CursorWordRight" => Ok(Action::CursorWordRight),
        "SelectAll" => Ok(Action::SelectAll),
        "Copy" => Ok(Action::Copy),
        "Cut" => Ok(Action::Cut),
        "Paste" => Ok(Action::Paste),
        "Undo" => Ok(Action::Undo),
        "Redo" => Ok(Action::Redo),
        "InsertNewline" => Ok(Action::InsertNewline),
        "Quit" => Ok(Action::Quit),
        "Screenshot" => Ok(Action::Screenshot),
        "DebugToggle" => Ok(Action::DebugToggle),
        "InterruptContext" => Ok(Action::InterruptContext { immediate: false }),
        _ => Err(format!("unknown action '{s}'")),
    }
}

fn parse_key_code(s: &str) -> Result<KeyCode, String> {
    match s {
        "KeyA" => Ok(KeyCode::KeyA),
        "KeyB" => Ok(KeyCode::KeyB),
        "KeyC" => Ok(KeyCode::KeyC),
        "KeyD" => Ok(KeyCode::KeyD),
        "KeyE" => Ok(KeyCode::KeyE),
        "KeyF" => Ok(KeyCode::KeyF),
        "KeyG" => Ok(KeyCode::KeyG),
        "KeyH" => Ok(KeyCode::KeyH),
        "KeyI" => Ok(KeyCode::KeyI),
        "KeyJ" => Ok(KeyCode::KeyJ),
        "KeyK" => Ok(KeyCode::KeyK),
        "KeyL" => Ok(KeyCode::KeyL),
        "KeyM" => Ok(KeyCode::KeyM),
        "KeyN" => Ok(KeyCode::KeyN),
        "KeyO" => Ok(KeyCode::KeyO),
        "KeyP" => Ok(KeyCode::KeyP),
        "KeyQ" => Ok(KeyCode::KeyQ),
        "KeyR" => Ok(KeyCode::KeyR),
        "KeyS" => Ok(KeyCode::KeyS),
        "KeyT" => Ok(KeyCode::KeyT),
        "KeyU" => Ok(KeyCode::KeyU),
        "KeyV" => Ok(KeyCode::KeyV),
        "KeyW" => Ok(KeyCode::KeyW),
        "KeyX" => Ok(KeyCode::KeyX),
        "KeyY" => Ok(KeyCode::KeyY),
        "KeyZ" => Ok(KeyCode::KeyZ),
        "Digit0" => Ok(KeyCode::Digit0),
        "Digit1" => Ok(KeyCode::Digit1),
        "Digit2" => Ok(KeyCode::Digit2),
        "Digit3" => Ok(KeyCode::Digit3),
        "Digit4" => Ok(KeyCode::Digit4),
        "Digit5" => Ok(KeyCode::Digit5),
        "Digit6" => Ok(KeyCode::Digit6),
        "Digit7" => Ok(KeyCode::Digit7),
        "Digit8" => Ok(KeyCode::Digit8),
        "Digit9" => Ok(KeyCode::Digit9),
        "F1" => Ok(KeyCode::F1),
        "F2" => Ok(KeyCode::F2),
        "F3" => Ok(KeyCode::F3),
        "F4" => Ok(KeyCode::F4),
        "F5" => Ok(KeyCode::F5),
        "F6" => Ok(KeyCode::F6),
        "F7" => Ok(KeyCode::F7),
        "F8" => Ok(KeyCode::F8),
        "F9" => Ok(KeyCode::F9),
        "F10" => Ok(KeyCode::F10),
        "F11" => Ok(KeyCode::F11),
        "F12" => Ok(KeyCode::F12),
        "Enter" => Ok(KeyCode::Enter),
        "Escape" => Ok(KeyCode::Escape),
        "Space" => Ok(KeyCode::Space),
        "Tab" => Ok(KeyCode::Tab),
        "Backspace" => Ok(KeyCode::Backspace),
        "Delete" => Ok(KeyCode::Delete),
        "Home" => Ok(KeyCode::Home),
        "End" => Ok(KeyCode::End),
        "ArrowLeft" => Ok(KeyCode::ArrowLeft),
        "ArrowRight" => Ok(KeyCode::ArrowRight),
        "ArrowUp" => Ok(KeyCode::ArrowUp),
        "ArrowDown" => Ok(KeyCode::ArrowDown),
        "BracketLeft" => Ok(KeyCode::BracketLeft),
        "BracketRight" => Ok(KeyCode::BracketRight),
        "Backslash" => Ok(KeyCode::Backslash),
        "Semicolon" => Ok(KeyCode::Semicolon),
        "Quote" => Ok(KeyCode::Quote),
        "Comma" => Ok(KeyCode::Comma),
        "Period" => Ok(KeyCode::Period),
        "Slash" => Ok(KeyCode::Slash),
        "Minus" => Ok(KeyCode::Minus),
        "Equal" => Ok(KeyCode::Equal),
        "Backquote" => Ok(KeyCode::Backquote),
        _ => Err(format!("unknown key code '{s}'")),
    }
}

fn parse_gamepad_button(s: &str) -> Result<GamepadButton, String> {
    match s {
        "South" => Ok(GamepadButton::South),
        "East" => Ok(GamepadButton::East),
        "North" => Ok(GamepadButton::North),
        "West" => Ok(GamepadButton::West),
        "Start" => Ok(GamepadButton::Start),
        "Select" => Ok(GamepadButton::Select),
        "DPadUp" => Ok(GamepadButton::DPadUp),
        "DPadDown" => Ok(GamepadButton::DPadDown),
        "DPadLeft" => Ok(GamepadButton::DPadLeft),
        "DPadRight" => Ok(GamepadButton::DPadRight),
        "LeftTrigger" => Ok(GamepadButton::LeftTrigger),
        "RightTrigger" => Ok(GamepadButton::RightTrigger),
        "LeftThumb" => Ok(GamepadButton::LeftThumb),
        "RightThumb" => Ok(GamepadButton::RightThumb),
        _ => Err(format!("unknown gamepad button '{s}'")),
    }
}
