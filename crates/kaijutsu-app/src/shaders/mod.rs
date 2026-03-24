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
use crate::ui::theme::Theme;

/// Plugin that registers shader effect materials and sync systems.
pub struct ShaderFxPlugin;

impl Plugin for ShaderFxPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(UiMaterialPlugin::<BlockFxMaterial>::default());
        app.add_systems(PostUpdate, sync_block_fx);
    }
}

/// Sync `BlockBorderStyle` + `Theme` → `BlockFxMaterial` parameters.
///
/// Maps border color, animation mode, corner radius, and text glow from
/// the theme to the shader's uniforms.
fn sync_block_fx(
    query: Query<
        (&MaterialNode<BlockFxMaterial>, Option<&BlockBorderStyle>),
        With<BlockCell>,
    >,
    mut fx_materials: ResMut<Assets<BlockFxMaterial>>,
    theme: Res<Theme>,
) {
    // Text glow from theme (same for all blocks)
    let tg_srgba = theme.text_glow_color.to_srgba();
    let target_tg_color = Vec4::new(tg_srgba.red, tg_srgba.green, tg_srgba.blue, tg_srgba.alpha);
    let target_tg_params = Vec4::new(theme.text_glow_radius, 0.0, 0.0, 0.0);

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
                theme.block_border_glow_radius,
                theme.block_border_glow_intensity,
                anim_mode,
                style.corner_radius,
            );

            // Only mutate if changed (avoids unnecessary GPU re-binds)
            if mat.glow_color != target_glow
                || mat.fx_params != target_params
                || mat.text_glow_color != target_tg_color
                || mat.text_glow_params != target_tg_params
            {
                mat.glow_color = target_glow;
                mat.fx_params = target_params;
                mat.text_glow_color = target_tg_color;
                mat.text_glow_params = target_tg_params;
            }
        } else {
            // No border → disable border effects, keep text glow
            let needs_update = mat.fx_params != Vec4::ZERO
                || mat.text_glow_color != target_tg_color
                || mat.text_glow_params != target_tg_params;
            if needs_update {
                mat.glow_color = Vec4::ZERO;
                mat.fx_params = Vec4::ZERO;
                mat.text_glow_color = target_tg_color;
                mat.text_glow_params = target_tg_params;
            }
        }
    }
}
