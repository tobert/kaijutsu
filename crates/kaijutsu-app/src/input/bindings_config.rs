//! Bindings configuration for Kaijutsu.
//!
//! Loads key bindings from `~/.config/kaijutsu/bindings.toml` at startup.
//! Errors are surfaced via [`BindingsConfigError`] / `LoadedBindings::entry_errors`
//! so callers can display them to the user rather than silently losing bindings.
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
use std::path::{Path, PathBuf};

use super::action::Action;
use super::binding::{Binding, Modifiers};
use super::context::InputContext;
use super::defaults::default_bindings;

// ============================================================================
// ERRORS
// ============================================================================

/// File-level failure loading bindings.toml. Per-entry failures are surfaced
/// via [`LoadedBindings::entry_errors`], not as `Err`.
#[derive(Debug)]
pub enum BindingsConfigError {
    /// Failed to read the file (permission denied, etc). File existence is
    /// NOT an error — missing file returns defaults as `Ok`.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// Top-level TOML syntax error.
    Parse { path: PathBuf, message: String },
}

impl std::fmt::Display for BindingsConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(f, "failed to read bindings at {}: {source}", path.display())
            }
            Self::Parse { path, message } => {
                write!(
                    f,
                    "failed to parse bindings at {}: {message}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for BindingsConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::Parse { .. } => None,
        }
    }
}

/// Successful load result. Individual binding entries that failed to parse
/// appear in `entry_errors` as human-readable messages; good bindings still
/// populate `bindings`.
#[derive(Debug)]
pub struct LoadedBindings {
    pub bindings: Vec<Binding>,
    pub entry_errors: Vec<String>,
}

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
/// Missing file → `Ok(LoadedBindings { bindings: default_bindings(), ... })`.
/// File read / top-level parse errors → `Err`. Per-entry errors are returned
/// in `LoadedBindings::entry_errors` so the caller can surface them.
pub fn load_bindings() -> Result<LoadedBindings, BindingsConfigError> {
    let Some(path) = bindings_file_path() else {
        info!("No config directory available, using default bindings");
        return Ok(LoadedBindings {
            bindings: default_bindings(),
            entry_errors: Vec::new(),
        });
    };
    load_bindings_from_path(&path)
}

/// Load bindings from an explicit path. Useful for tests.
///
/// The file MERGES over the built-in default table — it never replaces it.
/// A file entry with the same (source, modifiers, context) as a default
/// overrides that default; anything else is added. Defaults the file doesn't
/// mention survive. This is what lets the shipped table evolve: the old
/// behavior (file replaces table) froze every install at whatever default
/// dump `write_default_configs_if_missing` had written — a stale 2026-04
/// bindings.toml silently discarded months of new default bindings (found
/// live 2026-07-16 when none of the scene bindings existed in a running app).
pub fn load_bindings_from_path(path: &Path) -> Result<LoadedBindings, BindingsConfigError> {
    if !path.exists() {
        info!("Bindings not found at {:?}, using defaults", path);
        return Ok(LoadedBindings {
            bindings: default_bindings(),
            entry_errors: Vec::new(),
        });
    }

    let content = std::fs::read_to_string(path).map_err(|e| BindingsConfigError::Io {
        path: path.to_path_buf(),
        source: e,
    })?;

    let (user_bindings, entry_errors) =
        parse_bindings_toml(&content).map_err(|msg| BindingsConfigError::Parse {
            path: path.to_path_buf(),
            message: msg,
        })?;

    let bindings = merge_over_defaults(user_bindings);

    info!(
        "Loaded {} bindings from {:?} merged over defaults → {} total ({} per-entry errors)",
        content.matches("[[bindings]]").count(),
        path,
        bindings.len(),
        entry_errors.len()
    );
    Ok(LoadedBindings {
        bindings,
        entry_errors,
    })
}

/// Merge user bindings over the default table: same (source, modifiers,
/// context) → the user's action/label wins; otherwise the entry is appended.
fn merge_over_defaults(user: Vec<Binding>) -> Vec<Binding> {
    let mut merged = default_bindings();
    for u in user {
        if let Some(slot) = merged.iter_mut().find(|d| {
            d.source == u.source && d.modifiers == u.modifiers && d.context == u.context
        }) {
            *slot = u;
        } else {
            merged.push(u);
        }
    }
    merged
}

/// Parse a TOML string into a list of bindings plus per-entry error messages.
///
/// Top-level TOML syntax errors return `Err`. Individual entries that fail to
/// parse (unknown action, bad key code, unknown modifier) are returned as
/// messages in the `Vec<String>` — the caller is expected to surface them,
/// not the parser.
pub fn parse_bindings_toml(content: &str) -> Result<(Vec<Binding>, Vec<String>), String> {
    let parsed: BindingsToml =
        toml::from_str(content).map_err(|e| format!("TOML parse error: {e}"))?;

    let mut bindings = Vec::with_capacity(parsed.bindings.len());
    let mut entry_errors = Vec::new();
    for (idx, entry) in parsed.bindings.iter().enumerate() {
        match binding_from_entry(entry) {
            Ok(b) => bindings.push(b),
            Err(e) => entry_errors.push(format!("binding #{idx} ({:?}): {e}", entry.key)),
        }
    }

    Ok((bindings, entry_errors))
}

/// Serialize bindings to TOML format. Test-only since the override-template
/// change (2026-07-16): production never writes the table out any more, but
/// the round-trip test pins action/context string coverage for every enum
/// variant through this. A future "export the live InputMap" verb would
/// promote it back.
#[cfg(test)]
pub fn bindings_to_toml(bindings: &[Binding]) -> String {
    let entries: Vec<BindingEntry> = bindings.iter().map(binding_to_entry).collect();
    let toml_struct = BindingsTomlOut { bindings: entries };
    toml::to_string_pretty(&toml_struct)
        .expect("serializing BindingEntry is infallible for well-formed Binding values")
}

#[cfg(test)]
#[derive(Serialize)]
struct BindingsTomlOut {
    bindings: Vec<BindingEntry>,
}

// ============================================================================
// SERIALIZATION: Binding ↔ BindingEntry
// ============================================================================

#[cfg(test)]
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
    let modifiers = parse_modifiers(&e.modifiers)?;

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

#[cfg(test)]
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

fn parse_modifiers(s: &str) -> Result<Modifiers, String> {
    if s.is_empty() {
        return Ok(Modifiers::NONE);
    }
    let mut m = Modifiers::NONE;
    for part in s.split('+') {
        match part.trim() {
            "CTRL" => m.ctrl = true,
            "SHIFT" => m.shift = true,
            "ALT" => m.alt = true,
            "SUPER" => m.super_key = true,
            other => {
                return Err(format!(
                    "unknown modifier '{other}' (expected CTRL, SHIFT, ALT, or SUPER)"
                ));
            }
        }
    }
    Ok(m)
}

#[cfg(test)]
fn context_to_str(ctx: InputContext) -> String {
    match ctx {
        InputContext::Global => "Global",
        InputContext::Navigation => "Navigation",
        InputContext::TextInput => "TextInput",
        InputContext::Dialog => "Dialog",
        InputContext::RoomNav => "RoomNav",
        InputContext::WellZoomed => "WellZoomed",
        InputContext::PatchBayZoomed => "PatchBayZoomed",
        InputContext::StationZoomed => "StationZoomed",
        InputContext::FsnFly => "FsnFly",
    }
    .to_string()
}

fn parse_context(s: &str) -> Result<InputContext, String> {
    match s {
        "Global" => Ok(InputContext::Global),
        "Navigation" => Ok(InputContext::Navigation),
        "TextInput" => Ok(InputContext::TextInput),
        "Dialog" => Ok(InputContext::Dialog),
        "RoomNav" => Ok(InputContext::RoomNav),
        "WellZoomed" => Ok(InputContext::WellZoomed),
        "PatchBayZoomed" => Ok(InputContext::PatchBayZoomed),
        "StationZoomed" => Ok(InputContext::StationZoomed),
        "FsnFly" => Ok(InputContext::FsnFly),
        _ => Err(format!("unknown context '{s}'")),
    }
}

#[cfg(test)]
fn action_to_str(a: &Action) -> String {
    // Payloaded variants encode their payload after a ':' so the round-trip
    // through bindings.toml is lossless (before this, direction/etc was
    // silently dropped, and the TOML was unreadable on reload).
    match a {
        Action::CycleFocusForward => "CycleFocusForward".into(),
        Action::CycleFocusBackward => "CycleFocusBackward".into(),
        Action::FocusCompose => "FocusCompose".into(),
        Action::SummonChat => "SummonChat".into(),
        Action::ToggleSurface => "ToggleSurface".into(),
        Action::ToggleBlockExcluded => "ToggleBlockExcluded".into(),
        Action::PopLevel => "PopLevel".into(),
        Action::Activate => "Activate".into(),
        Action::FocusNextBlock => "FocusNextBlock".into(),
        Action::FocusPrevBlock => "FocusPrevBlock".into(),
        Action::FocusFirstBlock => "FocusFirstBlock".into(),
        Action::FocusLastBlock => "FocusLastBlock".into(),
        Action::CollapseToggle => "CollapseToggle".into(),
        Action::ScrollDelta(d) => format!("ScrollDelta:{d}"),
        Action::HalfPageUp => "HalfPageUp".into(),
        Action::HalfPageDown => "HalfPageDown".into(),
        Action::ScrollToEnd => "ScrollToEnd".into(),
        Action::ScrollToTop => "ScrollToTop".into(),
        Action::FocusPaneLeft => "FocusPaneLeft".into(),
        Action::FocusPaneDown => "FocusPaneDown".into(),
        Action::FocusPaneUp => "FocusPaneUp".into(),
        Action::FocusPaneRight => "FocusPaneRight".into(),
        Action::SplitVertical => "SplitVertical".into(),
        Action::SplitHorizontal => "SplitHorizontal".into(),
        Action::ClosePane => "ClosePane".into(),
        Action::GrowPane => "GrowPane".into(),
        Action::ShrinkPane => "ShrinkPane".into(),
        Action::TogglePreviousPaneFocus => "TogglePreviousPaneFocus".into(),
        Action::Submit => "Submit".into(),
        Action::Backspace => "Backspace".into(),
        Action::Delete => "Delete".into(),
        Action::CursorLeft => "CursorLeft".into(),
        Action::CursorRight => "CursorRight".into(),
        Action::CursorUp => "CursorUp".into(),
        Action::CursorDown => "CursorDown".into(),
        Action::CursorHome => "CursorHome".into(),
        Action::CursorEnd => "CursorEnd".into(),
        Action::CursorWordLeft => "CursorWordLeft".into(),
        Action::CursorWordRight => "CursorWordRight".into(),
        Action::Paste => "Paste".into(),
        Action::PastePrimary => "PastePrimary".into(),
        Action::InsertNewline => "InsertNewline".into(),
        Action::Quit => "Quit".into(),
        Action::Screenshot => "Screenshot".into(),
        Action::DebugToggle => "DebugToggle".into(),
        Action::InterruptContext { immediate } => {
            format!("InterruptContext:{immediate}")
        }
        Action::StepNext => "StepNext".into(),
        Action::StepPrev => "StepPrev".into(),
        Action::LevelUp => "LevelUp".into(),
        Action::LevelDown => "LevelDown".into(),
        Action::JumpSeat(n) => format!("JumpSeat:{n}"),
        Action::Promote => "Promote".into(),
        Action::Demote => "Demote".into(),
        Action::Conclude => "Conclude".into(),
        Action::PauseToggle => "PauseToggle".into(),
        Action::Archive => "Archive".into(),
        Action::Rescan => "Rescan".into(),
        Action::ToggleLegend => "ToggleLegend".into(),
        Action::SwitchToActiveSeat(n) => format!("SwitchToActiveSeat:{n}"),
        Action::SwitchToPreviousContext => "SwitchToPreviousContext".into(),
        Action::ActiveSeatStep(d) => format!("ActiveSeatStep:{d}"),
        Action::CloseAndDemoteContext => "CloseAndDemoteContext".into(),
        Action::GoToWell => "GoToWell".into(),
        Action::DetachToConversation => "DetachToConversation".into(),
        Action::SendLiteralPrefix => "SendLiteralPrefix".into(),
        Action::PromptContextRename => "PromptContextRename".into(),
        Action::PromptContextSwitch => "PromptContextSwitch".into(),
        Action::FlyAxis { x, y } => format!("FlyAxis:{x},{y}"),
        Action::FlyAltitude(v) => format!("FlyAltitude:{v}"),
    }
}

fn parse_action(s: &str) -> Result<Action, String> {
    // Payloaded actions use "Name:payload" — see `action_to_str`.
    if let Some((name, payload)) = s.split_once(':') {
        return parse_action_with_payload(name, payload);
    }
    match s {
        "CycleFocusForward" => Ok(Action::CycleFocusForward),
        "CycleFocusBackward" => Ok(Action::CycleFocusBackward),
        "FocusCompose" => Ok(Action::FocusCompose),
        "SummonChat" => Ok(Action::SummonChat),
        "ToggleSurface" => Ok(Action::ToggleSurface),
        "ToggleBlockExcluded" => Ok(Action::ToggleBlockExcluded),
        "PopLevel" => Ok(Action::PopLevel),
        // Pre-rename alias (bindings.toml written before 2026-07-16).
        "Unfocus" => Ok(Action::PopLevel),
        "Activate" => Ok(Action::Activate),
        "FocusNextBlock" => Ok(Action::FocusNextBlock),
        "FocusPrevBlock" => Ok(Action::FocusPrevBlock),
        "FocusFirstBlock" => Ok(Action::FocusFirstBlock),
        "FocusLastBlock" => Ok(Action::FocusLastBlock),
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
        "Paste" => Ok(Action::Paste),
        "PastePrimary" => Ok(Action::PastePrimary),
        "InsertNewline" => Ok(Action::InsertNewline),
        "Quit" => Ok(Action::Quit),
        "Screenshot" => Ok(Action::Screenshot),
        "DebugToggle" => Ok(Action::DebugToggle),
        "StepNext" => Ok(Action::StepNext),
        "StepPrev" => Ok(Action::StepPrev),
        "LevelUp" => Ok(Action::LevelUp),
        "LevelDown" => Ok(Action::LevelDown),
        "Promote" => Ok(Action::Promote),
        "Demote" => Ok(Action::Demote),
        "Conclude" => Ok(Action::Conclude),
        "PauseToggle" => Ok(Action::PauseToggle),
        "Archive" => Ok(Action::Archive),
        "Rescan" => Ok(Action::Rescan),
        "ToggleLegend" => Ok(Action::ToggleLegend),
        "SwitchToPreviousContext" => Ok(Action::SwitchToPreviousContext),
        "CloseAndDemoteContext" => Ok(Action::CloseAndDemoteContext),
        "GoToWell" => Ok(Action::GoToWell),
        "DetachToConversation" => Ok(Action::DetachToConversation),
        "SendLiteralPrefix" => Ok(Action::SendLiteralPrefix),
        "PromptContextRename" => Ok(Action::PromptContextRename),
        "PromptContextSwitch" => Ok(Action::PromptContextSwitch),
        "SwitchToActiveSeat" => Ok(Action::SwitchToActiveSeat(0)),
        "ActiveSeatStep" => Ok(Action::ActiveSeatStep(1)),
        // Payloaded variants without an explicit payload default to a
        // sensible zero/false value so tersely-typed user TOML keeps working.
        "InterruptContext" => Ok(Action::InterruptContext { immediate: false }),
        "ScrollDelta" => Ok(Action::ScrollDelta(0.0)),
        "JumpSeat" => Ok(Action::JumpSeat(0)),
        _ => Err(format!("unknown action '{s}'")),
    }
}

fn parse_action_with_payload(name: &str, payload: &str) -> Result<Action, String> {
    match name {
        "ScrollDelta" => {
            let delta: f32 = payload
                .parse()
                .map_err(|e| format!("ScrollDelta payload '{payload}' must be a float: {e}"))?;
            Ok(Action::ScrollDelta(delta))
        }
        "InterruptContext" => {
            let immediate: bool = payload
                .parse()
                .map_err(|e| format!("InterruptContext payload '{payload}' must be a bool: {e}"))?;
            Ok(Action::InterruptContext { immediate })
        }
        "JumpSeat" => {
            let n: usize = payload
                .parse()
                .map_err(|e| format!("JumpSeat payload '{payload}' must be a seat index: {e}"))?;
            Ok(Action::JumpSeat(n))
        }
        "SwitchToActiveSeat" => {
            let n: usize = payload.parse().map_err(|e| {
                format!("SwitchToActiveSeat payload '{payload}' must be a seat index: {e}")
            })?;
            Ok(Action::SwitchToActiveSeat(n))
        }
        "ActiveSeatStep" => {
            let d: i32 = payload.parse().map_err(|e| {
                format!("ActiveSeatStep payload '{payload}' must be a step: {e}")
            })?;
            Ok(Action::ActiveSeatStep(d))
        }
        "FlyAxis" => {
            let (x, y) = payload
                .split_once(',')
                .ok_or_else(|| format!("FlyAxis payload '{payload}' must be 'x,y'"))?;
            let x: f32 = x
                .parse()
                .map_err(|e| format!("FlyAxis x '{x}' must be a float: {e}"))?;
            let y: f32 = y
                .parse()
                .map_err(|e| format!("FlyAxis y '{y}' must be a float: {e}"))?;
            Ok(Action::FlyAxis { x, y })
        }
        "FlyAltitude" => {
            let v: f32 = payload
                .parse()
                .map_err(|e| format!("FlyAltitude payload '{payload}' must be a float: {e}"))?;
            Ok(Action::FlyAltitude(v))
        }
        _ => Err(format!(
            "action '{name}' does not take a payload (got ':{payload}')"
        )),
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

// ============================================================================
// TESTS
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    const ALL_ACTIONS: &[&str] = &[
        "CycleFocusForward",
        "CycleFocusBackward",
        "FocusCompose",
        "SummonChat",
        "ToggleSurface",
        "ToggleBlockExcluded",
        "PopLevel",
        "Activate",
        "FocusNextBlock",
        "FocusPrevBlock",
        "FocusFirstBlock",
        "FocusLastBlock",
        "CollapseToggle",
        "HalfPageUp",
        "HalfPageDown",
        "ScrollToEnd",
        "ScrollToTop",
        "FocusPaneLeft",
        "FocusPaneDown",
        "FocusPaneUp",
        "FocusPaneRight",
        "SplitVertical",
        "SplitHorizontal",
        "ClosePane",
        "GrowPane",
        "ShrinkPane",
        "TogglePreviousPaneFocus",
        "Submit",
        "Backspace",
        "Delete",
        "CursorLeft",
        "CursorRight",
        "CursorUp",
        "CursorDown",
        "CursorHome",
        "CursorEnd",
        "CursorWordLeft",
        "CursorWordRight",
        "Paste",
        "PastePrimary",
        "InsertNewline",
        "Quit",
        "Screenshot",
        "DebugToggle",
        "InterruptContext",
        "StepNext",
        "StepPrev",
        "LevelUp",
        "LevelDown",
        "JumpSeat",
        "Promote",
        "Demote",
        "Conclude",
        "PauseToggle",
        "Archive",
        "Rescan",
        "ToggleLegend",
        "SwitchToActiveSeat",
        "SwitchToPreviousContext",
        "ActiveSeatStep",
        "CloseAndDemoteContext",
        "GoToWell",
        "DetachToConversation",
        "SendLiteralPrefix",
        "PromptContextRename",
        "PromptContextSwitch",
    ];

    const ALL_KEYS: &[&str] = &[
        "KeyA",
        "KeyB",
        "KeyC",
        "KeyD",
        "KeyE",
        "KeyF",
        "KeyG",
        "KeyH",
        "KeyI",
        "KeyJ",
        "KeyK",
        "KeyL",
        "KeyM",
        "KeyN",
        "KeyO",
        "KeyP",
        "KeyQ",
        "KeyR",
        "KeyS",
        "KeyT",
        "KeyU",
        "KeyV",
        "KeyW",
        "KeyX",
        "KeyY",
        "KeyZ",
        "Digit0",
        "Digit1",
        "Digit2",
        "Digit3",
        "Digit4",
        "Digit5",
        "Digit6",
        "Digit7",
        "Digit8",
        "Digit9",
        "F1",
        "F2",
        "F3",
        "F4",
        "F5",
        "F6",
        "F7",
        "F8",
        "F9",
        "F10",
        "F11",
        "F12",
        "Enter",
        "Escape",
        "Space",
        "Tab",
        "Backspace",
        "Delete",
        "Home",
        "End",
        "ArrowLeft",
        "ArrowRight",
        "ArrowUp",
        "ArrowDown",
        "BracketLeft",
        "BracketRight",
        "Backslash",
        "Semicolon",
        "Quote",
        "Comma",
        "Period",
        "Slash",
        "Minus",
        "Equal",
        "Backquote",
    ];

    const ALL_GAMEPAD_BUTTONS: &[&str] = &[
        "South",
        "East",
        "North",
        "West",
        "Start",
        "Select",
        "DPadUp",
        "DPadDown",
        "DPadLeft",
        "DPadRight",
        "LeftTrigger",
        "RightTrigger",
        "LeftThumb",
        "RightThumb",
    ];

    const ALL_CONTEXTS: &[&str] = &["Global", "Navigation", "TextInput", "Dialog"];

    #[test]
    fn test_parse_action_all_variants() {
        for s in ALL_ACTIONS {
            parse_action(s).unwrap_or_else(|e| panic!("parse_action({s:?}) failed: {e}"));
        }
    }

    #[test]
    fn test_parse_action_rejects_unknown() {
        assert!(parse_action("Bogus").is_err());
        assert!(parse_action("").is_err());
        assert!(parse_action("focus_next_block").is_err(), "case-sensitive");
    }

    #[test]
    fn test_parse_key_code_all_variants() {
        for s in ALL_KEYS {
            parse_key_code(s).unwrap_or_else(|e| panic!("parse_key_code({s:?}) failed: {e}"));
        }
    }

    #[test]
    fn test_parse_key_code_rejects_unknown() {
        assert!(parse_key_code("Bogus").is_err());
        assert!(parse_key_code("keya").is_err(), "case-sensitive");
        assert!(parse_key_code("").is_err());
    }

    #[test]
    fn test_parse_gamepad_button_all_variants() {
        for s in ALL_GAMEPAD_BUTTONS {
            parse_gamepad_button(s)
                .unwrap_or_else(|e| panic!("parse_gamepad_button({s:?}) failed: {e}"));
        }
    }

    #[test]
    fn test_parse_gamepad_button_rejects_unknown() {
        assert!(parse_gamepad_button("A").is_err());
    }

    #[test]
    fn test_parse_modifiers_ok() {
        assert_eq!(parse_modifiers("").unwrap(), Modifiers::NONE);
        assert!(parse_modifiers("CTRL").unwrap().ctrl);
        let all = parse_modifiers("CTRL+SHIFT+ALT+SUPER").unwrap();
        assert!(all.ctrl && all.shift && all.alt && all.super_key);
    }

    #[test]
    fn test_parse_modifiers_rejects_unknown() {
        let err = parse_modifiers("CTRL+BOGUS").unwrap_err();
        assert!(err.contains("BOGUS"), "error should name bad token: {err}");
        assert!(
            parse_modifiers("SHIT").is_err(),
            "typo must not silently drop"
        );
    }

    #[test]
    fn test_parse_context_all_variants() {
        for s in ALL_CONTEXTS {
            parse_context(s).unwrap_or_else(|e| panic!("parse_context({s:?}) failed: {e}"));
        }
    }

    #[test]
    fn test_parse_context_rejects_unknown() {
        assert!(parse_context("global").is_err(), "case-sensitive");
        assert!(parse_context("Bogus").is_err());
    }

    #[test]
    fn test_defaults_round_trip() {
        let defaults = default_bindings();
        let toml_str = bindings_to_toml(&defaults);
        let (parsed, errors) = parse_bindings_toml(&toml_str).expect("default TOML must parse");
        assert!(
            errors.is_empty(),
            "default bindings must round-trip with zero entry errors: {errors:?}"
        );
        assert_eq!(
            parsed.len(),
            defaults.len(),
            "round-trip binding count must match"
        );
    }

    #[test]
    fn test_load_bindings_missing_file_returns_defaults() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bindings.toml");
        assert!(!path.exists());
        let loaded = load_bindings_from_path(&path).unwrap();
        assert_eq!(loaded.bindings.len(), default_bindings().len());
        assert!(loaded.entry_errors.is_empty());
    }

    #[test]
    fn test_file_merges_over_defaults_not_replaces() {
        // The 2026-04 snapshot trap: a file that is a stale full dump of old
        // defaults must not erase bindings the shipped table has since grown.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bindings.toml");
        // One override (Escape in Navigation → Quit, silly but detectable)
        // and one addition (F5 → Screenshot).
        std::fs::write(
            &path,
            r#"
[[bindings]]
key = "Escape"
context = "Navigation"
action = "Quit"
label = "user override"

[[bindings]]
key = "F5"
context = "Global"
action = "Screenshot"
label = "user addition"
"#,
        )
        .unwrap();
        let loaded = load_bindings_from_path(&path).unwrap();
        assert!(loaded.entry_errors.is_empty(), "{:?}", loaded.entry_errors);

        // Addition: exactly one more than the default table.
        assert_eq!(loaded.bindings.len(), default_bindings().len() + 1);

        // Override replaced the default row in place, not duplicated it.
        let esc_nav: Vec<_> = loaded
            .bindings
            .iter()
            .filter(|b| {
                b.source == super::super::binding::InputSource::Key(KeyCode::Escape)
                    && b.context == InputContext::Navigation
                    && b.modifiers == Modifiers::NONE
            })
            .collect();
        assert_eq!(esc_nav.len(), 1);
        assert_eq!(esc_nav[0].action, Action::Quit);

        // A default the file never mentioned survives (a scene binding —
        // the exact class the snapshot trap was silently deleting).
        assert!(loaded.bindings.iter().any(|b| {
            b.context == InputContext::WellZoomed && b.action == Action::StepNext
        }));
    }

    #[test]
    fn test_pre_rename_unfocus_alias_parses() {
        // Old dumps say "Unfocus"; they must land as PopLevel, not error.
        let (bindings, errors) = parse_bindings_toml(
            r#"
[[bindings]]
key = "Escape"
context = "Navigation"
action = "Unfocus"
"#,
        )
        .unwrap();
        assert!(errors.is_empty());
        assert_eq!(bindings[0].action, Action::PopLevel);
    }

    #[test]
    fn test_load_bindings_malformed_toml_returns_parse_err() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bindings.toml");
        std::fs::write(&path, "[invalid toml :::").unwrap();
        match load_bindings_from_path(&path) {
            Ok(_) => panic!("malformed bindings.toml must not return Ok"),
            Err(BindingsConfigError::Parse { path: p, .. }) => assert_eq!(p, path),
            Err(other) => panic!("expected Parse error, got {other}"),
        }
    }

    #[test]
    fn test_load_bindings_per_entry_errors_surface() {
        let toml = r#"
[[bindings]]
key = "KeyJ"
context = "Navigation"
action = "FocusNextBlock"

[[bindings]]
key = "KeyZ"
context = "Navigation"
action = "BogusAction"

[[bindings]]
key = "KeyK"
modifiers = "CTRL+BOGUS"
context = "Navigation"
action = "FocusPrevBlock"
"#;
        let (bindings, errors) = parse_bindings_toml(toml).unwrap();
        assert_eq!(bindings.len(), 1, "only the first binding should succeed");
        assert_eq!(errors.len(), 2, "two per-entry errors expected");
        assert!(errors.iter().any(|e| e.contains("BogusAction")));
        assert!(errors.iter().any(|e| e.contains("BOGUS")));
    }
}
