//! Input messages — the output of the dispatch system.
//!
//! Domain systems consume these instead of reading raw keyboard/gamepad input.

use bevy::prelude::*;

use super::action::Action;

/// A resolved action from the input dispatcher.
///
/// Emitted when a raw input matches a binding in an active context.
/// Domain systems read `MessageReader<ActionFired>` to react to user intent.
///
/// `context` is the binding context that matched at fire time. Scene
/// consumers MUST filter on it, not merely on their own run state: messages
/// buffer across frames, so a `PopLevel` fired under `Navigation` one frame
/// before a screen switch would otherwise be replayed by the next screen's
/// reader (an Esc in the conversation must never pop the room you just
/// entered). It also makes same-frame hand-offs unambiguous — a zoomed
/// station's Esc and the room's Esc are different contexts, so consumer
/// ordering no longer matters for correctness.
#[derive(Message, Clone, Debug, Reflect)]
pub struct ActionFired {
    pub action: Action,
    /// The binding context that matched when this action fired.
    pub context: super::context::InputContext,
}

impl ActionFired {
    pub fn new(action: Action, context: super::context::InputContext) -> Self {
        Self { action, context }
    }
}

/// Raw text that should be inserted into the focused text field.
///
/// Emitted by the dispatcher when input occurs in TextInput context
/// and no binding matches — the characters are plain text for the editor.
#[derive(Message, Clone, Debug, Reflect)]
pub struct TextInputReceived(pub String);

/// A pressed keyboard event routed to the active keyboard grab.
///
/// `dispatch_input` is the ONLY reader of Bevy's raw `KeyboardInput` stream.
/// While a [`super::context::KeyboardGrab`] is active, pressed keys that
/// don't match a Global binding are re-emitted as `GrabbedKey` for the grab
/// owner (compose VimMachine, vi editor session) to consume. This replaces
/// the old pattern of multiple systems reading `KeyboardInput` in parallel
/// and relying on run-conditions to avoid double-handling.
#[derive(Message, Clone, Debug)]
pub struct GrabbedKey(pub bevy::input::keyboard::KeyboardInput);

/// `Ctrl+A a` — deliver one literal Ctrl+A to the focused vi surface.
///
/// A synthetic `GrabbedKey` can't carry it: both grab owners read live
/// modifier state from `ButtonInput` (Ctrl is long released by the time the
/// chord resolves), so the literal travels as its own message. The compose
/// VimMachine feeds it as a Ctrl+A TerminalKey; the editor pushes `<C-a>`
/// into the key pipe. With no vi surface focused it goes nowhere, by design.
#[derive(Message, Clone, Copy, Debug)]
pub struct LiteralPrefix;

/// Analog input state from gamepad sticks.
///
/// Updated each frame by the dispatcher's gamepad polling.
/// Domain systems can read this for smooth scrolling/panning.
///
/// BRP-queryable: `world_get_resources("kaijutsu_app::input::events::AnalogInput")`
#[derive(Resource, Default, Reflect)]
#[reflect(Resource)]
pub struct AnalogInput {
    /// Left stick X (-1.0 to 1.0)
    pub left_stick_x: f32,
    /// Left stick Y (-1.0 to 1.0)
    pub left_stick_y: f32,
    /// Right stick X (-1.0 to 1.0)
    pub right_stick_x: f32,
    /// Right stick Y (-1.0 to 1.0)
    pub right_stick_y: f32,
}
