//! Shader effects for Kaijutsu UI
//!
//! Hybrid Vello + shader architecture: Vello renders structural content
//! (text, fieldset borders with label gaps) to per-block textures. Shader
//! materials add GPU-native post-processing (SDF glow, animation overlays).

pub mod block_fx_material;

use bevy::prelude::*;

pub use block_fx_material::BlockFxMaterial;

use crate::cell::block_border::{BlockBorderStyle, BorderAnimation};
use crate::cell::BlockCell;

/// Plugin that registers shader effect materials and sync systems.
pub struct ShaderFxPlugin;

impl Plugin for ShaderFxPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(UiMaterialPlugin::<BlockFxMaterial>::default());
        app.add_systems(PostUpdate, sync_block_fx);
    }
}

/// Sync `BlockBorderStyle` → `BlockFxMaterial` parameters.
///
/// Maps border color, animation mode, and corner radius from the Vello
/// fieldset styling to the shader's glow uniforms.
fn sync_block_fx(
    query: Query<
        (&MaterialNode<BlockFxMaterial>, Option<&BlockBorderStyle>),
        With<BlockCell>,
    >,
    mut fx_materials: ResMut<Assets<BlockFxMaterial>>,
) {
    for (mat_node, border) in query.iter() {
        let Some(mat) = fx_materials.get_mut(&mat_node.0) else {
            continue;
        };

        if let Some(style) = border {
            let srgba = style.color.to_srgba();
            let target_glow = Vec4::new(srgba.red, srgba.green, srgba.blue, srgba.alpha);
            let anim_mode = match style.animation {
                BorderAnimation::None => 0.0,
                BorderAnimation::Breathe => 1.0,
                BorderAnimation::Pulse => 2.0,
                BorderAnimation::Chase => 3.0,
            };
            let target_params = Vec4::new(
                6.0, // glow_radius (pixels of falloff)
                0.25, // glow_intensity
                anim_mode,
                style.corner_radius,
            );

            // Only mutate if changed (avoids unnecessary GPU re-binds)
            if mat.glow_color != target_glow || mat.fx_params != target_params {
                mat.glow_color = target_glow;
                mat.fx_params = target_params;
            }
        } else {
            // No border → disable effects
            if mat.fx_params != Vec4::ZERO {
                mat.glow_color = Vec4::ZERO;
                mat.fx_params = Vec4::ZERO;
            }
        }
    }
}
