//! Binding types — map input sources to actions in specific contexts.

use bevy::prelude::*;

use super::action::Action;
use super::context::InputContext;

/// What physical input triggered this binding.
#[derive(Clone, Debug, PartialEq, Reflect)]
pub enum InputSource {
    /// A keyboard key
    Key(KeyCode),
    /// A gamepad button
    GamepadButton(GamepadButton),
    // Phase 6: GamepadAxis with threshold
    // Phase 6+: MidiCC, Touch
}

/// Modifier key state required for a binding to match.
#[derive(Clone, Debug, Default, PartialEq, Reflect)]
pub struct Modifiers {
    pub ctrl: bool,
    pub shift: bool,
    pub alt: bool,
    pub super_key: bool,
}

impl Modifiers {
    pub const NONE: Modifiers = Modifiers {
        ctrl: false,
        shift: false,
        alt: false,
        super_key: false,
    };

    pub const CTRL: Modifiers = Modifiers {
        ctrl: true,
        shift: false,
        alt: false,
        super_key: false,
    };

    pub const SHIFT: Modifiers = Modifiers {
        ctrl: false,
        shift: true,
        alt: false,
        super_key: false,
    };

    pub const ALT: Modifiers = Modifiers {
        ctrl: false,
        shift: false,
        alt: true,
        super_key: false,
    };

    pub const CTRL_SHIFT: Modifiers = Modifiers {
        ctrl: true,
        shift: true,
        alt: false,
        super_key: false,
    };

    /// Check if the currently-held keys match these modifiers.
    pub fn matches(&self, keys: &ButtonInput<KeyCode>) -> bool {
        let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
        let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
        let alt = keys.pressed(KeyCode::AltLeft) || keys.pressed(KeyCode::AltRight);
        let super_key = keys.pressed(KeyCode::SuperLeft) || keys.pressed(KeyCode::SuperRight);

        self.ctrl == ctrl && self.shift == shift && self.alt == alt && self.super_key == super_key
    }
}

/// A single input binding: source + modifiers + context → action.
#[derive(Clone, Debug, Reflect)]
pub struct Binding {
    /// What input triggers this binding
    pub source: InputSource,
    /// Required modifier keys
    pub modifiers: Modifiers,
    /// When this binding is active
    pub context: InputContext,
    /// What action it fires
    pub action: Action,
    /// Human-readable description (for hint widget + Claude introspection)
    pub description: String,
    /// For multi-key sequences (g→t): the prefix key that must be pending.
    /// None = direct binding, Some(key) = second key in a sequence.
    pub sequence_prefix: Option<InputSource>,
}

impl Binding {
    /// Create a simple key binding with no modifiers or sequence.
    pub fn key(key: KeyCode, context: InputContext, action: Action, desc: impl Into<String>) -> Self {
        Self {
            source: InputSource::Key(key),
            modifiers: Modifiers::NONE,
            context,
            action,
            description: desc.into(),
            sequence_prefix: None,
        }
    }

    /// Create a binding with modifiers.
    pub fn key_mod(
        key: KeyCode,
        modifiers: Modifiers,
        context: InputContext,
        action: Action,
        desc: impl Into<String>,
    ) -> Self {
        Self {
            source: InputSource::Key(key),
            modifiers,
            context,
            action,
            description: desc.into(),
            sequence_prefix: None,
        }
    }

    /// Create a sequence binding (e.g. g→t).
    pub fn key_seq(
        prefix: KeyCode,
        key: KeyCode,
        context: InputContext,
        action: Action,
        desc: impl Into<String>,
    ) -> Self {
        Self {
            source: InputSource::Key(key),
            modifiers: Modifiers::NONE,
            context,
            action,
            description: desc.into(),
            sequence_prefix: Some(InputSource::Key(prefix)),
        }
    }

    /// Create a sequence binding with modifiers on the second key (e.g. g→Shift+T).
    pub fn key_seq_mod(
        prefix: KeyCode,
        key: KeyCode,
        modifiers: Modifiers,
        context: InputContext,
        action: Action,
        desc: impl Into<String>,
    ) -> Self {
        Self {
            source: InputSource::Key(key),
            modifiers,
            context,
            action,
            description: desc.into(),
            sequence_prefix: Some(InputSource::Key(prefix)),
        }
    }
}
