//! Block FX material — MSDF + shader post-processing.
//!
//! `BlockFxMaterial` is a `UiMaterial` that displays the MSDF-rendered block
//! texture and adds GPU-native effects (SDF border glow, animation, cursor beam).

use bevy::prelude::*;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

/// Post-processing material for conversation block textures.
///
/// # Uniforms
///
/// - `texture` / `sampler`: The MSDF-rendered block texture.
/// - `glow_color`: RGBA color for the border glow effect (linear).
/// - `fx_params`: `[glow_radius, glow_intensity, animation_mode, corner_radius]`
/// - `text_glow_color`: RGBA color for text halo.
/// - `text_glow_params`: `[radius_px, 0, 0, 0]`
/// - `cursor_params`: `[x_uv, y_uv, width_uv, height_uv]` — cursor beam rect in UV space.
///   All zero = no cursor. Color comes from `cursor_color`.
/// - `cursor_color`: RGBA color for the cursor beam (linear).
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct BlockFxMaterial {
    #[texture(0)]
    #[sampler(1)]
    pub texture: Handle<Image>,

    #[uniform(2)]
    pub glow_color: Vec4,

    /// [glow_radius, glow_intensity, animation_mode, corner_radius]
    #[uniform(3)]
    pub fx_params: Vec4,

    /// Text glow color (RGBA, linear color space).
    #[uniform(4)]
    pub text_glow_color: Vec4,

    /// Text glow parameters: [radius_px, 0, 0, 0]. radius=0 disables.
    #[uniform(5)]
    pub text_glow_params: Vec4,

    /// Cursor beam rect in UV space: [x, y, width, height]. All zero = disabled.
    #[uniform(6)]
    pub cursor_params: Vec4,

    /// Cursor beam color (RGBA, linear color space).
    #[uniform(7)]
    pub cursor_color: Vec4,
}

impl Default for BlockFxMaterial {
    fn default() -> Self {
        Self {
            texture: Handle::default(),
            glow_color: Vec4::ZERO,
            fx_params: Vec4::ZERO,
            text_glow_color: Vec4::ZERO,
            text_glow_params: Vec4::ZERO,
            cursor_params: Vec4::ZERO,
            cursor_color: Vec4::ZERO,
        }
    }
}

impl UiMaterial for BlockFxMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/block_fx.wgsl".into()
    }
}
