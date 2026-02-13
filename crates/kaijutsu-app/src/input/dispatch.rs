//! Input dispatcher — the ONE system that reads raw input and emits actions.
//!
//! Single-pass dispatch with context priority. For each raw input event:
//! 1. Check sequence completion (if pending)
//! 2. Check sequence start (is this key a known prefix?)
//! 3. Check direct binding match (active contexts, source + modifiers)
//! 4. If no match AND in TextInput context → emit TextInputReceived
//!
//! Mouse wheel → ScrollDelta action.
//! Gamepad → Phase 6.

use bevy::input::keyboard::KeyboardInput;
use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;

use super::action::Action;
use super::binding::InputSource;
use super::context::{ActiveInputContexts, InputContext};
use super::events::{ActionFired, TextInputReceived};
use super::map::InputMap;
use super::sequence::SequenceState;

/// The main input dispatch system.
///
/// Runs every frame. Reads raw keyboard events + mouse wheel and emits
/// `ActionFired` or `TextInputReceived` messages. Domain systems consume
/// those messages instead of reading raw input directly.
pub fn dispatch_input(
    mut keyboard: MessageReader<KeyboardInput>,
    mut mouse_wheel: MessageReader<MouseWheel>,
    keys: Res<ButtonInput<KeyCode>>,
    input_map: Res<InputMap>,
    active_contexts: Res<ActiveInputContexts>,
    mut sequence: ResMut<SequenceState>,
    mut action_writer: MessageWriter<ActionFired>,
    mut text_writer: MessageWriter<TextInputReceived>,
) {
    // Clear expired sequences
    if sequence.pending.is_some() && sequence.is_expired(input_map.sequence_timeout_ms) {
        sequence.clear();
    }

    // --- Mouse wheel → ScrollDelta ---
    for event in mouse_wheel.read() {
        let delta = match event.unit {
            bevy::input::mouse::MouseScrollUnit::Line => event.y * 40.0,
            bevy::input::mouse::MouseScrollUnit::Pixel => event.y,
        };
        if delta.abs() > 0.001 {
            action_writer.write(ActionFired(Action::ScrollDelta(-delta)));
        }
    }

    // --- Keyboard ---
    for event in keyboard.read() {
        if !event.state.is_pressed() {
            continue;
        }

        let key = event.key_code;

        // 1. Check sequence completion
        if sequence.pending.is_some() {
            if let Some(action) = find_sequence_match(
                key,
                &keys,
                &sequence,
                &input_map,
                &active_contexts,
            ) {
                sequence.clear();
                action_writer.write(ActionFired(action));
                continue;
            }
            // Key didn't complete a sequence — clear and fall through to direct match
            sequence.clear();
        }

        // 2. Check if this key starts a sequence
        if is_sequence_prefix(key, &input_map, &active_contexts) && no_modifiers_held(&keys) {
            sequence.start(InputSource::Key(key));
            continue;
        }

        // 3. Check direct binding match
        if let Some(action) = find_direct_match(key, &keys, &input_map, &active_contexts) {
            action_writer.write(ActionFired(action));
            continue;
        }

        // 4. No match in TextInput context → emit text
        if active_contexts.contains(InputContext::TextInput)
            && let Some(ref text) = event.text
        {
            let s = text.as_str();
            if !s.is_empty() && s.chars().all(|c| !c.is_control()) {
                text_writer.write(TextInputReceived(s.to_string()));
            }
        }
    }
}

/// Check if no modifiers are held (for sequence prefix detection).
fn no_modifiers_held(keys: &ButtonInput<KeyCode>) -> bool {
    !(keys.pressed(KeyCode::ControlLeft)
        || keys.pressed(KeyCode::ControlRight)
        || keys.pressed(KeyCode::ShiftLeft)
        || keys.pressed(KeyCode::ShiftRight)
        || keys.pressed(KeyCode::AltLeft)
        || keys.pressed(KeyCode::AltRight)
        || keys.pressed(KeyCode::SuperLeft)
        || keys.pressed(KeyCode::SuperRight))
}

/// Check if a key is a known sequence prefix in any active context.
fn is_sequence_prefix(
    key: KeyCode,
    input_map: &InputMap,
    active_contexts: &ActiveInputContexts,
) -> bool {
    let source = InputSource::Key(key);
    input_map.bindings.iter().any(|binding| {
        binding.sequence_prefix.as_ref() == Some(&source)
            && active_contexts.contains(binding.context)
    })
}

/// Find a binding that completes a pending sequence.
fn find_sequence_match(
    key: KeyCode,
    keys: &ButtonInput<KeyCode>,
    sequence: &SequenceState,
    input_map: &InputMap,
    active_contexts: &ActiveInputContexts,
) -> Option<Action> {
    for binding in &input_map.bindings {
        // Must be a sequence binding
        let Some(ref prefix) = binding.sequence_prefix else {
            continue;
        };

        // Prefix must match pending
        if !sequence.matches_prefix(prefix) {
            continue;
        }

        // Second key + modifiers must match
        if let InputSource::Key(bind_key) = &binding.source
            && *bind_key == key
            && binding.modifiers.matches(keys)
            && active_contexts.contains(binding.context)
        {
            return Some(binding.action.clone());
        }
    }
    None
}

/// Find a direct (non-sequence) binding match.
fn find_direct_match(
    key: KeyCode,
    keys: &ButtonInput<KeyCode>,
    input_map: &InputMap,
    active_contexts: &ActiveInputContexts,
) -> Option<Action> {
    // Check specific contexts first (higher priority), then Global
    // This ensures TextInput Enter → Submit beats Global Enter (if any)
    let mut best_match: Option<(usize, &super::binding::Binding)> = None;

    for binding in &input_map.bindings {
        // Skip sequence bindings (handled separately)
        if binding.sequence_prefix.is_some() {
            continue;
        }

        // Source must match
        if let InputSource::Key(bind_key) = &binding.source {
            if *bind_key != key {
                continue;
            }
        } else {
            continue; // Not a key binding
        }

        // Modifiers must match
        if !binding.modifiers.matches(keys) {
            continue;
        }

        // Context must be active
        if !active_contexts.contains(binding.context) {
            continue;
        }

        // Priority: prefer non-Global over Global (more specific wins)
        let priority = context_priority(binding.context);
        if let Some((best_prio, _)) = &best_match {
            if priority > *best_prio {
                best_match = Some((priority, binding));
            }
        } else {
            best_match = Some((priority, binding));
        }
    }

    best_match.map(|(_, binding)| binding.action.clone())
}

/// Context priority for conflict resolution (higher = more specific = wins).
fn context_priority(ctx: InputContext) -> usize {
    match ctx {
        InputContext::Global => 0,
        InputContext::Navigation => 1,
        InputContext::TextInput => 1,
        InputContext::Constellation => 1,
        InputContext::Dialog => 2, // Dialog beats everything
        InputContext::Dashboard => 1,
    }
}
