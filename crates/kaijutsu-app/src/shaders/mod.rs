//! Shader effects for Kaijutsu UI
//!
//! Custom `UiMaterial` implementations for specialized rendering effects.
//! Most constellation rendering has moved to Vello 2D.

use bevy::prelude::*;

/// Plugin that registers shader effect materials.
///
/// Note: ConstellationCardMaterial was removed in favor of Vello 2.5D rendering.
pub struct ShaderFxPlugin;

impl Plugin for ShaderFxPlugin {
    fn build(&self, _app: &mut App) {
        // Placeholder - additional shader materials can be added here as needed
    }
}
