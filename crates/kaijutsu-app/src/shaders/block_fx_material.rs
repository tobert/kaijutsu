//! Block FX material — hybrid Vello + shader post-processing.
//!
//! `BlockFxMaterial` is a `UiMaterial` that displays the Vello-rendered block
//! texture and adds GPU-native effects (edge glow, animation). The Vello
//! fieldset borders provide the structural geometry; this shader adds the
//! soft visual layer that Vello can't do (SDF glow falloff, per-pixel animation).

use bevy::prelude::*;
use bevy::render::render_resource::AsBindGroup;
use bevy::shader::ShaderRef;

/// Post-processing material for conversation block textures.
///
/// Binds the Vello-rendered texture (text + fieldset borders) and adds
/// SDF-based edge glow with animation.
///
/// # Uniforms
///
/// - `texture` / `sampler`: The Vello-rendered block texture.
/// - `glow_color`: RGBA color for the glow effect (linear color space).
/// - `fx_params`: `[glow_radius, glow_intensity, animation_mode, corner_radius]`
///   - `glow_radius`: Pixel width of glow falloff (0 = disabled, fast path).
///   - `glow_intensity`: Peak brightness multiplier.
///   - `animation_mode`: 0=none, 1=breathe, 2=pulse, 3=chase.
///   - `corner_radius`: Rounded rect corner radius for SDF alignment.
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
}

impl Default for BlockFxMaterial {
    fn default() -> Self {
        Self {
            texture: Handle::default(),
            glow_color: Vec4::ZERO,
            // glow_radius=0 → shader fast path (pure passthrough)
            fx_params: Vec4::ZERO,
            text_glow_color: Vec4::ZERO,
            text_glow_params: Vec4::ZERO,
        }
    }
}

impl UiMaterial for BlockFxMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/block_fx.wgsl".into()
    }
}
