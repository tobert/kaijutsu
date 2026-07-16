//! Input dispatcher — the ONE system that reads raw input and emits actions.
//!
//! `dispatch_input` is the only reader of Bevy's raw `KeyboardInput` stream
//! (docs/input.md "As built"). Per pressed key:
//! 1. Check a direct binding match (active contexts, source + modifiers).
//!    While a [`KeyboardGrab`] is active only Global bindings are matchable,
//!    so F1/F12/tiling work everywhere without leaking keys into text.
//! 2. Unmatched keys under a grab are routed to the grab owner (compose
//!    VimMachine, vi editor session) as [`GrabbedKey`] messages.
//!
//! Mouse wheel → ScrollDelta action.
//! Gamepad buttons → direct binding match.
//! Gamepad analog sticks → AnalogInput resource + continuous actions.
//! Held fly keys / left stick → FlyAxis/FlyAltitude while FsnFly is active.

use bevy::input::gamepad::Gamepad;
use bevy::input::keyboard::KeyboardInput;
use bevy::input::mouse::MouseWheel;
use bevy::prelude::*;

use super::action::Action;
use super::binding::{InputSource, Modifiers};
use super::context::{ActiveInputContexts, InputContext, KeyboardGrab};
use super::events::{ActionFired, AnalogInput, GrabbedKey, LiteralPrefix};
use super::map::InputMap;

/// The main input dispatch system.
///
/// Runs every frame. Reads raw keyboard events, mouse wheel, and gamepad
/// input, then emits `ActionFired` or `GrabbedKey` messages. Domain systems
/// consume those messages instead of reading raw input.
///
/// OS key repeat is allowed through to grab owners (holding Backspace,
/// h/j/k/l scrubbing) but filtered out for action bindings to prevent
/// accidental double-fires (e.g. Alt+V creating two splits).
pub fn dispatch_input(
    mut keyboard: MessageReader<KeyboardInput>,
    mut mouse_wheel: MessageReader<MouseWheel>,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    gamepads: Query<&Gamepad>,
    time: Res<Time>,
    input_map: Res<InputMap>,
    active_contexts: Res<ActiveInputContexts>,
    grab: Res<KeyboardGrab>,
    mut prefix: ResMut<super::prefix::PrefixState>,
    mut action_writer: MessageWriter<ActionFired>,
    mut grab_writer: MessageWriter<GrabbedKey>,
    mut literal_writer: MessageWriter<LiteralPrefix>,
    mut analog_input: ResMut<AnalogInput>,
) {
    // An armed prefix that outlived its window lapses quietly. Gated on
    // armed() (an &self read, no DerefMut) so the resource only shows
    // change-detection while a prefix is actually pending — the footer
    // hint (`ui/dock.rs::update_hints`) keys off that flag.
    if prefix.armed() {
        prefix.tick_timeout();
    }

    // --- Mouse wheel → ScrollDelta ---
    for event in mouse_wheel.read() {
        let delta = match event.unit {
            bevy::input::mouse::MouseScrollUnit::Line => event.y * 40.0,
            bevy::input::mouse::MouseScrollUnit::Pixel => event.y,
        };
        if delta.abs() > 0.001 {
            action_writer.write(ActionFired::new(
                Action::ScrollDelta(-delta),
                InputContext::Global,
            ));
        }
    }

    // --- Middle-click → PRIMARY paste (xterm-style, docs/input.md) ---
    // Compose only: paste needs a text surface to land in.
    if *grab == KeyboardGrab::ComposeVim && mouse_buttons.just_pressed(MouseButton::Middle) {
        action_writer.write(ActionFired::new(
            Action::PastePrimary,
            InputContext::TextInput,
        ));
    }

    // --- Keyboard ---
    let grabbed = *grab != KeyboardGrab::None;

    for event in keyboard.read() {
        if !event.state.is_pressed() {
            continue;
        }

        let key = event.key_code;
        let is_repeat = event.repeat;

        // --- Ctrl+A prefix machine (docs/input.md) ---
        // First stage, every surface: prefix wins over grabs and bindings.
        // While armed, every key is swallowed — resolved, flashed-unbound,
        // or (for bare modifiers) ignored — nothing leaks to the layer below.
        let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
        let shift = keys.pressed(KeyCode::ShiftLeft) || keys.pressed(KeyCode::ShiftRight);
        if prefix.armed() {
            if !super::prefix::is_bare_modifier(key) && !is_repeat {
                match super::prefix::resolve_chord(key, ctrl, shift) {
                    // The literal travels on its own channel to the grab
                    // owners (see `LiteralPrefix`), not as an action.
                    Some(Action::SendLiteralPrefix) => {
                        literal_writer.write(LiteralPrefix);
                    }
                    Some(action) => {
                        action_writer.write(ActionFired::new(action, InputContext::Global));
                    }
                    None => {
                        if key != KeyCode::Escape {
                            info!("prefix: no binding for Ctrl+A {:?}", key);
                        }
                    }
                }
                prefix.disarm();
            }
            continue;
        }
        if ctrl && key == KeyCode::KeyA && !is_repeat {
            prefix.arm();
            continue;
        }

        // 1. Check a direct binding match. Under a grab, only Global-context
        // bindings are considered — matched keys are consumed here and never
        // reach the grab owner, so Alt+V in compose splits a pane instead of
        // typing a stray 'v'.
        //
        // Always check for a match — if bound, consume the event even on
        // repeat to prevent fallthrough (Bug: action leak on key repeat).
        let matched = if grabbed {
            find_direct_match(key, &keys, &input_map, |ctx| ctx == InputContext::Global)
        } else {
            find_direct_match(key, &keys, &input_map, |ctx| active_contexts.contains(ctx))
        };
        if let Some((action, ctx)) = matched {
            if !is_repeat {
                action_writer.write(ActionFired::new(action, ctx));
            }
            continue;
        }

        // 2. Route unmatched keys to the grab owner (repeats included — vim
        // scrubbing and held Backspace depend on them).
        if grabbed {
            grab_writer.write(GrabbedKey(event.clone()));
        }
    }

    // --- Held-key fly axes (FsnFly) ---
    // Continuous movement doesn't fit just_pressed bindings; poll held keys
    // like the analog-stick lane below. Consumers scale by their own dt.
    if active_contexts.contains(InputContext::FsnFly) && !grabbed {
        let mut axis = Vec2::ZERO;
        if keys.pressed(KeyCode::KeyW) || keys.pressed(KeyCode::ArrowUp) {
            axis.y += 1.0;
        }
        if keys.pressed(KeyCode::KeyS) || keys.pressed(KeyCode::ArrowDown) {
            axis.y -= 1.0;
        }
        if keys.pressed(KeyCode::KeyA) || keys.pressed(KeyCode::ArrowLeft) {
            axis.x -= 1.0;
        }
        if keys.pressed(KeyCode::KeyD) || keys.pressed(KeyCode::ArrowRight) {
            axis.x += 1.0;
        }
        if axis != Vec2::ZERO {
            action_writer.write(ActionFired::new(
                Action::FlyAxis {
                    x: axis.x,
                    y: axis.y,
                },
                InputContext::FsnFly,
            ));
        }

        let mut altitude = 0.0_f32;
        if keys.pressed(KeyCode::PageUp) || keys.pressed(KeyCode::Equal) {
            altitude += 1.0;
        }
        if keys.pressed(KeyCode::PageDown) || keys.pressed(KeyCode::Minus) {
            altitude -= 1.0;
        }
        if altitude != 0.0 {
            action_writer.write(ActionFired::new(
                Action::FlyAltitude(altitude),
                InputContext::FsnFly,
            ));
        }
    }

    // --- Gamepad buttons ---
    // Use first connected gamepad (single-player). Multi-gamepad later.
    if let Some(gamepad) = gamepads.iter().next() {
        for binding in &input_map.bindings {
            if let InputSource::GamepadButton(btn) = &binding.source
                && gamepad.just_pressed(*btn)
                && binding.modifiers == Modifiers::NONE
                && active_contexts.contains(binding.context)
            {
                action_writer.write(ActionFired::new(binding.action.clone(), binding.context));
            }
        }

        // --- Analog stick → AnalogInput resource ---
        let left = gamepad.left_stick();
        let right = gamepad.right_stick();
        analog_input.left_stick_x = left.x;
        analog_input.left_stick_y = left.y;
        analog_input.right_stick_x = right.x;
        analog_input.right_stick_y = right.y;

        // --- Analog stick → continuous actions ---
        // Scale by delta_secs so speed is frame-rate independent.
        // At 60fps: dt≈0.016, at 144fps: dt≈0.007 — same pixels/second.
        const THRESHOLD: f32 = 0.2;
        let dt = time.delta_secs();

        // Left stick → scroll (Navigation context)
        // 500 px/s at full deflection
        if active_contexts.contains(InputContext::Navigation) && left.y.abs() > THRESHOLD {
            let scroll_speed = -left.y * 500.0 * dt;
            action_writer.write(ActionFired::new(
                Action::ScrollDelta(scroll_speed),
                InputContext::Navigation,
            ));
        }

        // Left stick → fly (FsnFly context); consumer applies speed * dt.
        if active_contexts.contains(InputContext::FsnFly)
            && (left.x.abs() > THRESHOLD || left.y.abs() > THRESHOLD)
        {
            action_writer.write(ActionFired::new(
                Action::FlyAxis {
                    x: left.x,
                    y: left.y,
                },
                InputContext::FsnFly,
            ));
        }
    } else {
        // No gamepad connected — zero out
        if analog_input.left_stick_x != 0.0
            || analog_input.left_stick_y != 0.0
            || analog_input.right_stick_x != 0.0
            || analog_input.right_stick_y != 0.0
        {
            *analog_input = AnalogInput::default();
        }
    }
}

/// Find a direct binding match among contexts accepted by `context_active`.
fn find_direct_match(
    key: KeyCode,
    keys: &ButtonInput<KeyCode>,
    input_map: &InputMap,
    context_active: impl Fn(InputContext) -> bool,
) -> Option<(Action, InputContext)> {
    // Check specific contexts first (higher priority), then Global
    // This ensures TextInput Enter → Submit beats Global Enter (if any)
    let mut best_match: Option<(usize, &super::binding::Binding)> = None;

    for binding in &input_map.bindings {
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
        if !context_active(binding.context) {
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

    best_match.map(|(_, binding)| (binding.action.clone(), binding.context))
}

/// Context priority for conflict resolution (higher = more specific = wins).
fn context_priority(ctx: InputContext) -> usize {
    match ctx {
        InputContext::Global => 0,
        InputContext::Navigation
        | InputContext::TextInput
        | InputContext::RoomNav
        | InputContext::WellZoomed
        | InputContext::PatchBayZoomed
        | InputContext::StationZoomed
        | InputContext::FsnFly => 1,
        InputContext::Dialog => 2, // Dialog beats everything
    }
}
