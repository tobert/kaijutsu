//! Shader effects for Kaijutsu UI
//!
//! MSDF renders text to per-block textures. Shader materials add GPU-native
//! post-processing (SDF border glow, animation, text halo, cursor beam).

pub mod block_fx_material;

use bevy::prelude::*;

pub use block_fx_material::BlockFxMaterial;

use crate::cell::block_border::{BlockBorderStyle, BorderAnimation};
use crate::cell::BlockCell;
use crate::input::FocusArea;
use crate::ui::theme::Theme;
use crate::view::block_render::BlockScene;
use crate::view::components::{MsdfOverlayText, OverlayCursorGeometry};

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
/// For overlay entities, also syncs cursor geometry → cursor_params uniform.
fn sync_block_fx(
    query: Query<
        (
            &MaterialNode<BlockFxMaterial>,
            Option<&BlockBorderStyle>,
            Has<MsdfOverlayText>,
            Option<&OverlayCursorGeometry>,
            Option<&BlockScene>,
        ),
        Or<(With<BlockCell>, With<MsdfOverlayText>)>,
    >,
    mut fx_materials: ResMut<Assets<BlockFxMaterial>>,
    theme: Res<Theme>,
    focus: Res<FocusArea>,
) {
    let tg_srgba = theme.text_glow_color.to_srgba();
    let target_tg_color = Vec4::new(tg_srgba.red, tg_srgba.green, tg_srgba.blue, tg_srgba.alpha);
    let target_tg_params = Vec4::new(theme.text_glow_radius, 0.0, 0.0, 0.0);

    let show_cursor = matches!(*focus, FocusArea::Compose);

    for (mat_node, border, is_overlay, cursor_geom, block_scene) in query.iter() {
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

            let (glow_radius, glow_intensity) = if is_overlay {
                (
                    theme.compose_palette_glow_radius,
                    theme.compose_palette_glow_intensity,
                )
            } else {
                (
                    theme.block_border_glow_radius,
                    theme.block_border_glow_intensity,
                )
            };

            let target_params = Vec4::new(
                glow_radius,
                glow_intensity,
                anim_mode,
                style.corner_radius,
            );

            mat.glow_color = target_glow;
            mat.fx_params = target_params;
            mat.text_glow_color = target_tg_color;
            mat.text_glow_params = target_tg_params;
        } else {
            mat.glow_color = Vec4::ZERO;
            mat.fx_params = Vec4::ZERO;
            mat.text_glow_color = target_tg_color;
            mat.text_glow_params = target_tg_params;
        }

        // Cursor beam (overlay only)
        if is_overlay && show_cursor {
            if let (Some(geom), Some(scene)) = (cursor_geom, block_scene) {
                if geom.height > 0.0 && scene.built_width > 0.0 && scene.built_height > 0.0 {
                    let beam_width = 2.0;
                    // Convert pixel coords → UV [0,1]
                    let cx = geom.x as f32 / scene.built_width;
                    let cy = geom.y as f32 / scene.built_height;
                    let cw = beam_width / scene.built_width;
                    let ch = geom.height as f32 / scene.built_height;

                    mat.cursor_params = Vec4::new(cx, cy, cw, ch);

                    let c = theme.cursor_insert;
                    mat.cursor_color = Vec4::new(c.x, c.y, c.z, c.w);
                } else {
                    mat.cursor_params = Vec4::ZERO;
                    mat.cursor_color = Vec4::ZERO;
                }
            }
        } else {
            mat.cursor_params = Vec4::ZERO;
            mat.cursor_color = Vec4::ZERO;
        }
    }
}
