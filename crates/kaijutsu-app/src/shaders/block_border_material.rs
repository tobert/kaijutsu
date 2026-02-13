//! Block border material — single-node shader border for conversation blocks.
//!
//! Adapted from `ChasingBorderMaterial` with support for multiple border kinds
//! (full, top-accent, dashed) and animation modes (static, chase, pulse, breathe).

use bevy::{
    prelude::*,
    render::render_resource::AsBindGroup,
    shader::ShaderRef,
};

use crate::cell::block_border::{BlockBorderStyle, BorderAnimation, BorderKind};
use crate::ui::theme::{color_to_vec4, Theme};

/// Single-node border material for block cells.
///
/// Uniforms:
/// - `color`: Border color (RGBA).
/// - `params`: [thickness_px, corner_radius_px, glow_radius, glow_intensity].
/// - `time`: [elapsed, _, _, _].
/// - `mode`: [animation_mode, animation_speed, dash_count, border_kind].
///   - animation_mode: 0=static, 1=chase, 2=pulse, 3=breathe
///   - border_kind: 0=full, 1=top_accent, 2=dashed
/// - `dimensions`: [width_px, height_px, _, _].
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone)]
pub struct BlockBorderMaterial {
    #[uniform(0)]
    pub color: Vec4,
    #[uniform(1)]
    pub params: Vec4,
    #[uniform(2)]
    pub time: Vec4,
    #[uniform(3)]
    pub mode: Vec4,
    #[uniform(4)]
    pub dimensions: Vec4,
}

impl Default for BlockBorderMaterial {
    fn default() -> Self {
        Self {
            color: Vec4::new(0.89, 0.79, 0.49, 0.8), // Amber
            params: Vec4::new(1.5, 4.0, 0.15, 0.6),   // thickness, radius, glow_r, glow_i
            time: Vec4::ZERO,
            mode: Vec4::new(0.0, 1.0, 12.0, 0.0),     // static, speed, dash_count, full
            dimensions: Vec4::new(200.0, 50.0, 0.0, 0.0),
        }
    }
}

impl BlockBorderMaterial {
    /// Create material from a BlockBorderStyle and theme.
    pub fn from_style(style: &BlockBorderStyle, theme: &Theme) -> Self {
        Self::from_style_with_dimensions(style, theme, Vec4::new(200.0, 50.0, 0.0, 0.0))
    }

    /// Create material preserving existing dimensions.
    pub fn from_style_with_dimensions(style: &BlockBorderStyle, theme: &Theme, dimensions: Vec4) -> Self {
        let color = color_to_vec4(style.color);

        let animation_mode = match style.animation {
            BorderAnimation::None => 0.0,
            BorderAnimation::Chase => 1.0,
            BorderAnimation::Pulse => 2.0,
            BorderAnimation::Breathe => 3.0,
        };

        let border_kind = match style.kind {
            BorderKind::Full => 0.0,
            BorderKind::TopAccent => 1.0,
            BorderKind::Dashed => 2.0,
        };

        let animation_speed = match style.animation {
            BorderAnimation::Chase => theme.effect_chase_speed,
            BorderAnimation::Pulse => 2.0,
            BorderAnimation::Breathe => theme.effect_breathe_speed,
            BorderAnimation::None => 0.0,
        };

        Self {
            color,
            params: Vec4::new(
                style.thickness,
                style.corner_radius,
                theme.block_border_glow_radius,
                theme.block_border_glow_intensity,
            ),
            time: Vec4::ZERO,
            mode: Vec4::new(animation_mode, animation_speed, 12.0, border_kind),
            dimensions,
        }
    }
}

impl UiMaterial for BlockBorderMaterial {
    fn fragment_shader() -> ShaderRef {
        "shaders/block_border.wgsl".into()
    }
}
