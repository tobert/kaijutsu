//! Input messages — the output of the dispatch system.
//!
//! Domain systems consume these instead of reading raw keyboard/gamepad input.

use bevy::prelude::*;

use super::action::Action;

/// A resolved action from the input dispatcher.
///
/// Emitted when a raw input matches a binding in an active context.
/// Domain systems read `MessageReader<ActionFired>` to react to user intent.
#[derive(Message, Clone, Debug, Reflect)]
pub struct ActionFired(pub Action);

/// Raw text that should be inserted into the focused text field.
///
/// Emitted by the dispatcher when input occurs in TextInput context
/// and no binding matches — the characters are plain text for the editor.
#[derive(Message, Clone, Debug, Reflect)]
pub struct TextInputReceived(pub String);

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
