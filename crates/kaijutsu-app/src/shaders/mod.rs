//! Shader effects for Kaijutsu UI
//!
//! Custom `UiMaterial` implementations:
//! - `ConstellationCardMaterial` - Rounded card for constellation nodes

use bevy::{
    prelude::*,
    render::render_resource::AsBindGroup,
    shader::ShaderRef,
};

use crate::ui::screen::Screen;

/// Plugin that registers shader effect materials.
pub struct ShaderFxPlugin;

impl Plugin for ShaderFxPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(UiMaterialPlugin::<ConstellationCardMaterial>::default())
            // Only update card time when in constellation (cards visible).
            .add_systems(
                Update,
                update_card_shader_time.run_if(in_state(Screen::Constellation)),
            );
    }
}

/// Update time uniform on constellation card materials (only when cards are visible).
fn update_card_shader_time(
    time: Res<Time>,
    mut card_materials: ResMut<Assets<ConstellationCardMaterial>>,
) {
    let t = time.elapsed_secs();
    for (_, mat) in card_materials.iter_mut() {
        mat.time.x = t;
    }
}

// ============================================================================
// CONSTELLATION CARD MATERIAL
// ============================================================================

/// Card node for constellation context visualization.
///
/// Renders a rounded rectangle with agent-colored border, soft outer glow,
/// dark fill interior, and an activity indicator dot in the top-right corner.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct ConstellationCardMaterial {
    /// Border/agent color (RGBA)
    #[uniform(0)]
    pub color: Vec4,
    /// Parameters: x=thickness_px, y=corner_radius_px, z=glow_radius, w=glow_intensity
    #[uniform(1)]
    pub params: Vec4,
    /// Time: x=elapsed_time
    #[uniform(2)]
    pub time: Vec4,
    /// Mode: x=activity_dot_r, y=activity_dot_g, z=activity_dot_b, w=reserved
    #[uniform(3)]
    pub mode: Vec4,
    /// Dimensions: x=width_px, y=height_px, z=opacity, w=focused(0/1)
    #[uniform(4)]
    pub dimensions: Vec4,
}

impl Default for ConstellationCardMaterial {
    fn default() -> Self {
        Self {
            color: Vec4::new(0.49, 0.98, 1.0, 0.8),
            params: Vec4::new(1.5, 6.0, 0.3, 0.5),
            time: Vec4::ZERO,
            mode: Vec4::ZERO,
            dimensions: Vec4::new(180.0, 130.0, 1.0, 0.0),
        }
    }
}

impl UiMaterial for ConstellationCardMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/constellation_card.wgsl".into()
    }
}
