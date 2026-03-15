//! Rhai-based bindings loader for Kaijutsu.
//!
//! Loads key bindings from `~/.config/kaijutsu/bindings.rhai` at startup.
//! Falls back to `default_bindings()` if the file doesn't exist or has errors.
//! Hot-reloads when the file changes (polls mtime every 2 seconds).
//!
//! ## Rhai API
//!
//! ```rhai
//! // Simple key binding
//! binding("KeyJ", "Navigation", "FocusNextBlock", "Next block")
//!
//! // Key + modifiers
//! binding_mod("KeyD", "CTRL", "Navigation", "HalfPageDown", "Half page down")
//!
//! // Gamepad button
//! gamepad("South", "Navigation", "Activate", "Activate")
//!
//! // Start from defaults and customize
//! let b = default_bindings();
//! b = b.filter(|x| !(x["key"] == "KeyQ" && x["context"] == "Navigation"));
//! b.push(binding("KeyQ", "Navigation", "Quit", "Quit"));
//! b
//! ```

use bevy::input::gamepad::GamepadButton;
use bevy::prelude::*;
use rhai::{Array, Dynamic, Engine, Map};
use std::path::PathBuf;

use super::action::Action;
use super::binding::{Binding, Modifiers};
use super::context::InputContext;
use super::defaults::default_bindings;

// ============================================================================
// FILE PATH
// ============================================================================

/// Get the bindings config file path (~/.config/kaijutsu/bindings.rhai).
pub fn bindings_file_path() -> Option<PathBuf> {
    dirs::config_dir().map(|p| p.join("kaijutsu").join("bindings.rhai"))
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

    let script = match std::fs::read_to_string(&path) {
        Ok(s) => {
            info!("Loaded bindings from {:?}", path);
            s
        }
        Err(e) => {
            warn!("Failed to read bindings {:?}: {}", path, e);
            return default_bindings();
        }
    };

    match parse_bindings_script(&script) {
        Ok(bindings) => bindings,
        Err(e) => {
            warn!("Failed to parse bindings: {}", e);
            warn!("Falling back to default bindings");
            default_bindings()
        }
    }
}

/// Parse a bindings script string into a Vec<Binding>.
pub fn parse_bindings_script(script: &str) -> Result<Vec<Binding>, String> {
    let engine = build_engine();
    let result: Dynamic = engine
        .eval::<Dynamic>(script)
        .map_err(|e| format!("Rhai eval error: {e}"))?;
    parse_bindings_from_dynamic(result)
}

/// Parse bindings from a Rhai Dynamic return value.
///
/// Used by the shared config engine to extract bindings from a bindings script's
/// return value without requiring a separate engine instance.
pub fn parse_bindings_from_dynamic(result: Dynamic) -> Result<Vec<Binding>, String> {
    let arr = result
        .try_cast::<Array>()
        .ok_or_else(|| "bindings.rhai must return an array".to_string())?;

    let mut bindings = Vec::with_capacity(arr.len());
    for item in arr {
        let map = item
            .try_cast::<Map>()
            .ok_or_else(|| "each binding must be a map".to_string())?;
        match binding_from_map(map) {
            Ok(b) => bindings.push(b),
            Err(e) => warn!("Skipping invalid binding: {}", e),
        }
    }

    info!("Parsed {} bindings from script", bindings.len());
    Ok(bindings)
}

// ============================================================================
// RHAI ENGINE SETUP
// ============================================================================

/// Register binding helper functions on a Rhai engine.
///
/// Exposes `binding()`, `binding_mod()`, `gamepad()`, and `default_bindings()`
/// to scripts. Called by both the local `build_engine()` and the shared
/// `config::build_app_engine()` so the same helpers are available in all scripts.
pub fn register_binding_fns(engine: &mut Engine, defaults: Vec<Binding>) {
    // Register `default_bindings()` → Array of maps (the Rust defaults)
    engine.register_fn("default_bindings", move || -> Array {
        defaults
            .iter()
            .map(binding_to_map)
            .map(Dynamic::from)
            .collect()
    });

    // Register `binding(key, context, action, label)` → Map
    engine.register_fn("binding", |key: &str, context: &str, action: &str, label: &str| -> Map {
        let mut m = Map::new();
        m.insert("key".into(), Dynamic::from(key.to_string()));
        m.insert("modifiers".into(), Dynamic::from(String::new()));
        m.insert("context".into(), Dynamic::from(context.to_string()));
        m.insert("action".into(), Dynamic::from(action.to_string()));
        m.insert("gamepad".into(), Dynamic::from(false));
        m.insert("label".into(), Dynamic::from(label.to_string()));
        m
    });

    // Register `binding_mod(key, mods, context, action, label)` → Map
    engine.register_fn(
        "binding_mod",
        |key: &str, mods: &str, context: &str, action: &str, label: &str| -> Map {
            let mut m = Map::new();
            m.insert("key".into(), Dynamic::from(key.to_string()));
            m.insert("modifiers".into(), Dynamic::from(mods.to_string()));
            m.insert("context".into(), Dynamic::from(context.to_string()));
            m.insert("action".into(), Dynamic::from(action.to_string()));
            m.insert("gamepad".into(), Dynamic::from(false));
            m.insert("label".into(), Dynamic::from(label.to_string()));
            m
        },
    );

    // Register `gamepad(button, context, action, label)` → Map
    engine.register_fn(
        "gamepad",
        |button: &str, context: &str, action: &str, label: &str| -> Map {
            let mut m = Map::new();
            m.insert("key".into(), Dynamic::from(button.to_string()));
            m.insert("modifiers".into(), Dynamic::from(String::new()));
            m.insert("context".into(), Dynamic::from(context.to_string()));
            m.insert("action".into(), Dynamic::from(action.to_string()));
            m.insert("gamepad".into(), Dynamic::from(true));
            m.insert("label".into(), Dynamic::from(label.to_string()));
            m
        },
    );
}

fn build_engine() -> Engine {
    let mut engine = Engine::new();
    register_binding_fns(&mut engine, default_bindings());
    engine
}

// ============================================================================
// SERIALIZATION: Binding ↔ Rhai Map
// ============================================================================

fn binding_to_map(b: &Binding) -> Map {
    let mut m = Map::new();
    let (key_str, is_gamepad) = match &b.source {
        super::binding::InputSource::Key(k) => (format!("{:?}", k), false),
        super::binding::InputSource::GamepadButton(btn) => (format!("{:?}", btn), true),
    };
    m.insert("key".into(), Dynamic::from(key_str));
    m.insert("modifiers".into(), Dynamic::from(modifiers_to_str(&b.modifiers)));
    m.insert("context".into(), Dynamic::from(context_to_str(b.context)));
    m.insert("action".into(), Dynamic::from(action_to_str(&b.action)));
    m.insert("gamepad".into(), Dynamic::from(is_gamepad));
    m.insert("label".into(), Dynamic::from(b.description.clone()));
    m
}

fn binding_from_map(m: Map) -> Result<Binding, String> {
    let key_str = get_str(&m, "key")?;
    let mods_str = get_str(&m, "modifiers").unwrap_or_default();
    let ctx_str = get_str(&m, "context")?;
    let action_str = get_str(&m, "action")?;
    let is_gamepad = m.get("gamepad").and_then(|v| v.as_bool().ok()).unwrap_or(false);
    let label = get_str(&m, "label").unwrap_or_default();

    let context = parse_context(&ctx_str)?;
    let action = parse_action(&action_str)?;
    let modifiers = parse_modifiers(&mods_str);

    let source = if is_gamepad {
        super::binding::InputSource::GamepadButton(parse_gamepad_button(&key_str)?)
    } else {
        super::binding::InputSource::Key(parse_key_code(&key_str)?)
    };

    Ok(Binding { source, modifiers, context, action, description: label })
}

fn get_str(m: &Map, key: &str) -> Result<String, String> {
    m.get(key)
        .ok_or_else(|| format!("missing field '{key}'"))?
        .clone()
        .try_cast::<String>()
        .ok_or_else(|| format!("field '{key}' must be a string"))
}

fn modifiers_to_str(m: &Modifiers) -> String {
    let mut parts = Vec::new();
    if m.ctrl { parts.push("CTRL"); }
    if m.shift { parts.push("SHIFT"); }
    if m.alt { parts.push("ALT"); }
    if m.super_key { parts.push("SUPER"); }
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
            other => warn!("Unknown modifier '{other}' in bindings.rhai"),
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
        Action::SummonShell => "SummonShell",
        Action::CycleModeRing => "CycleModeRing",
        Action::Unfocus => "Unfocus",
        Action::Activate => "Activate",
        Action::FocusNextBlock => "FocusNextBlock",
        Action::FocusPrevBlock => "FocusPrevBlock",
        Action::FocusFirstBlock => "FocusFirstBlock",
        Action::FocusLastBlock => "FocusLastBlock",
        Action::ExpandBlock => "ExpandBlock",
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
        "SummonShell" => Ok(Action::SummonShell),
        "CycleModeRing" => Ok(Action::CycleModeRing),
        "Unfocus" => Ok(Action::Unfocus),
        "Activate" => Ok(Action::Activate),
        "FocusNextBlock" => Ok(Action::FocusNextBlock),
        "FocusPrevBlock" => Ok(Action::FocusPrevBlock),
        "FocusFirstBlock" => Ok(Action::FocusFirstBlock),
        "FocusLastBlock" => Ok(Action::FocusLastBlock),
        "ExpandBlock" => Ok(Action::ExpandBlock),
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
        // InterruptContext defaults immediate=false — press count escalates in handle_escape
        "InterruptContext" => Ok(Action::InterruptContext { immediate: false }),
        _ => Err(format!("unknown action '{s}'")),
    }
}

fn parse_key_code(s: &str) -> Result<KeyCode, String> {
    // Map string names to KeyCode variants
    match s {
        "KeyA" => Ok(KeyCode::KeyA), "KeyB" => Ok(KeyCode::KeyB),
        "KeyC" => Ok(KeyCode::KeyC), "KeyD" => Ok(KeyCode::KeyD),
        "KeyE" => Ok(KeyCode::KeyE), "KeyF" => Ok(KeyCode::KeyF),
        "KeyG" => Ok(KeyCode::KeyG), "KeyH" => Ok(KeyCode::KeyH),
        "KeyI" => Ok(KeyCode::KeyI), "KeyJ" => Ok(KeyCode::KeyJ),
        "KeyK" => Ok(KeyCode::KeyK), "KeyL" => Ok(KeyCode::KeyL),
        "KeyM" => Ok(KeyCode::KeyM), "KeyN" => Ok(KeyCode::KeyN),
        "KeyO" => Ok(KeyCode::KeyO), "KeyP" => Ok(KeyCode::KeyP),
        "KeyQ" => Ok(KeyCode::KeyQ), "KeyR" => Ok(KeyCode::KeyR),
        "KeyS" => Ok(KeyCode::KeyS), "KeyT" => Ok(KeyCode::KeyT),
        "KeyU" => Ok(KeyCode::KeyU), "KeyV" => Ok(KeyCode::KeyV),
        "KeyW" => Ok(KeyCode::KeyW), "KeyX" => Ok(KeyCode::KeyX),
        "KeyY" => Ok(KeyCode::KeyY), "KeyZ" => Ok(KeyCode::KeyZ),
        "Digit0" => Ok(KeyCode::Digit0), "Digit1" => Ok(KeyCode::Digit1),
        "Digit2" => Ok(KeyCode::Digit2), "Digit3" => Ok(KeyCode::Digit3),
        "Digit4" => Ok(KeyCode::Digit4), "Digit5" => Ok(KeyCode::Digit5),
        "Digit6" => Ok(KeyCode::Digit6), "Digit7" => Ok(KeyCode::Digit7),
        "Digit8" => Ok(KeyCode::Digit8), "Digit9" => Ok(KeyCode::Digit9),
        "F1" => Ok(KeyCode::F1), "F2" => Ok(KeyCode::F2),
        "F3" => Ok(KeyCode::F3), "F4" => Ok(KeyCode::F4),
        "F5" => Ok(KeyCode::F5), "F6" => Ok(KeyCode::F6),
        "F7" => Ok(KeyCode::F7), "F8" => Ok(KeyCode::F8),
        "F9" => Ok(KeyCode::F9), "F10" => Ok(KeyCode::F10),
        "F11" => Ok(KeyCode::F11), "F12" => Ok(KeyCode::F12),
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

// TODO: live bindings reload should come through the kernel VFS, not the
//   filesystem directly. When bindings are editable inside Kaijutsu, the
//   config lives at e.g. /docs/config/bindings in the CRDT store. Changes
//   flow through the kernel and trigger a reload event — or the file is
//   exposed via v9fs/fuse so all edits are kernel-mediated anyway.
//   The XDG file is write-only from Kaijutsu's perspective (persistence +
//   re-hydration on next launch). Default config writing is handled by
//   crate::config::write_default_configs_if_missing().
