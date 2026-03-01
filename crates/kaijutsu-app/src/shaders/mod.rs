//! Shader effects for Kaijutsu UI
//!
//! Custom `UiMaterial` implementations:
//! - `CursorBeamMaterial` - Glowing cursor beam (beam/block/underline modes)
//! - `ConstellationCardMaterial` - Rounded card for constellation nodes

use bevy::{
    prelude::*,
    render::render_resource::AsBindGroup,
    shader::ShaderRef,
};

/// Plugin that registers shader effect materials.
pub struct ShaderFxPlugin;

impl Plugin for ShaderFxPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins((
            UiMaterialPlugin::<CursorBeamMaterial>::default(),
            UiMaterialPlugin::<ConstellationCardMaterial>::default(),
        ))
        .add_systems(Update, update_shader_time);
    }
}

/// Update time uniforms on shader materials.
fn update_shader_time(
    time: Res<Time>,
    mut cursor_materials: ResMut<Assets<CursorBeamMaterial>>,
    mut card_materials: ResMut<Assets<ConstellationCardMaterial>>,
) {
    let t = time.elapsed_secs();
    for (_, mat) in cursor_materials.iter_mut() {
        mat.time.x = t;
    }
    for (_, mat) in card_materials.iter_mut() {
        mat.time.x = t;
    }
}

// ============================================================================
// CURSOR BEAM MATERIAL
// ============================================================================

/// Glowing cursor beam with cyberpunk energy effects.
///
/// Supports three modes:
/// - Beam (0): Vertical line cursor (insert mode)
/// - Block (1): Filled block cursor (normal mode)
/// - Underline (2): Horizontal underline cursor
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct CursorBeamMaterial {
    /// Cursor color (RGBA)
    #[uniform(0)]
    pub color: Vec4,
    /// Parameters: x=glow_width, y=intensity, z=pulse_speed, w=blink_rate
    #[uniform(1)]
    pub params: Vec4,
    /// Time: x=elapsed_time, y=mode (0=beam, 1=block, 2=underline)
    #[uniform(2)]
    pub time: Vec4,
}

/// Cursor display modes
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorMode {
    Beam = 0,      // Vertical line (insert mode)
    Block = 1,     // Filled block (normal mode)
    #[allow(dead_code)]
    Underline = 2, // Bottom line (future: replace mode)
}

impl Default for CursorBeamMaterial {
    fn default() -> Self {
        Self {
            color: Vec4::new(1.0, 0.5, 0.75, 0.95), // Hot pink
            params: Vec4::new(0.25, 1.2, 2.0, 0.0),
            time: Vec4::new(0.0, 1.0, 0.0, 0.0), // time, mode (default block)
        }
    }
}

impl UiMaterial for CursorBeamMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/cursor_beam.wgsl".into()
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
