//! Shader effects for Kaijutsu UI
//!
//! MSDF renders text to per-block textures. Shader materials add GPU-native
//! post-processing (SDF border glow, animation, text halo, cursor beam).

pub mod block_fx_material;
pub mod chord_material;
pub mod terrace_ring_material;
pub mod track_ray_material;
pub mod well_card_material;
pub mod well_rings_material;

use bevy::prelude::*;

pub use block_fx_material::BlockFxMaterial;
pub use chord_material::ChordMaterial;
pub use terrace_ring_material::TerraceRingMaterial;
pub use track_ray_material::TrackRayMaterial;
pub use well_card_material::WellCardMaterial;
pub use well_rings_material::WellRingsMaterial;

use crate::cell::block_border::{
    BlockBorderStyle, BlockExcludedState, BorderAnimation, BorderKind, BorderLabelMetrics,
};
use crate::cell::BlockCell;
use crate::input::FocusArea;
use crate::ui::theme::Theme;
use crate::view::ui_rtt::UiRttTexture;
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
            Option<&BorderLabelMetrics>,
            Has<MsdfOverlayText>,
            Has<crate::view::shell_dock::MsdfShellDockText>,
            Option<&OverlayCursorGeometry>,
            &UiRttTexture,
            Option<&BlockExcludedState>,
        ),
        Or<(With<BlockCell>, With<MsdfOverlayText>, With<crate::view::shell_dock::MsdfShellDockText>)>,
    >,
    mut fx_materials: ResMut<Assets<BlockFxMaterial>>,
    theme: Res<Theme>,
    focus: Res<FocusArea>,
) {
    let tg_srgba = theme.text_glow_color.to_srgba();
    let target_tg_color = Vec4::new(tg_srgba.red, tg_srgba.green, tg_srgba.blue, tg_srgba.alpha);
    // .y is packed per-block below (excluded_flag)
    let tg_radius = theme.text_glow_radius;

    let show_cursor = matches!(*focus, FocusArea::Compose);

    for (mat_node, border, label_metrics, is_chat_overlay, is_shell_dock, cursor_geom, rtt, excluded_state) in query.iter() {
        let is_overlay = is_chat_overlay || is_shell_dock;
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
            // .y = excluded flag for gutter indicator
            let excluded_flag = excluded_state.map_or(0.0, |e| if e.0 { 1.0 } else { 0.0 });
            mat.text_glow_params = Vec4::new(tg_radius, excluded_flag, 0.0, 0.0);

            // Border stroke uniforms
            let border_kind = match style.kind {
                BorderKind::Full => 1.0,
                BorderKind::TopAccent => 2.0,
                BorderKind::Dashed => 3.0,
                BorderKind::OpenBottom => 4.0,
                BorderKind::OpenTop => 5.0,
            };
            // .z/.w carry label border insets (0 = use default 1px AA inset)
            let (inset_top, inset_bottom) = label_metrics
                .map(|lm| (lm.border_inset_top, lm.border_inset_bottom))
                .unwrap_or((0.0, 0.0));
            mat.border_stroke = Vec4::new(style.thickness, border_kind, inset_top, inset_bottom);
            mat.border_insets = Vec4::new(
                style.padding.top,
                style.padding.bottom,
                style.padding.left,
                style.padding.right,
            );
            mat.border_color = target_glow;

            // Label gap metrics
            if let Some(lm) = label_metrics {
                mat.label_gaps = Vec4::new(lm.top_gap_x0, lm.top_gap_x1, lm.bottom_gap_x0, lm.bottom_gap_x1);
            } else {
                mat.label_gaps = Vec4::ZERO;
            }
        } else {
            mat.glow_color = Vec4::ZERO;
            mat.fx_params = Vec4::ZERO;
            mat.text_glow_color = target_tg_color;
            let excluded_flag = excluded_state.map_or(0.0, |e| if e.0 { 1.0 } else { 0.0 });
            mat.text_glow_params = Vec4::new(tg_radius, excluded_flag, 0.0, 0.0);
            mat.border_stroke = Vec4::ZERO;
            mat.border_insets = Vec4::ZERO;
            mat.border_color = Vec4::ZERO;
            mat.label_gaps = Vec4::ZERO;
        }

        // Cursor (overlay only) — width and color depend on vim mode. Shared
        // with the editor surface via `cursor_selection_uniforms`.
        let (cp, cc, sp, sc) = match (is_overlay && show_cursor, cursor_geom) {
            (true, Some(geom)) => {
                cursor_selection_uniforms(geom, rtt.built_width, rtt.built_height, &theme)
            }
            _ => (Vec4::ZERO, Vec4::ZERO, Vec4::ZERO, Vec4::ZERO),
        };
        mat.cursor_params = cp;
        mat.cursor_color = cc;
        mat.selection_params = sp;
        mat.selection_color = sc;
    }
}

/// Compute the cursor + selection material uniforms from cursor geometry, in UV
/// space: `(cursor_params, cursor_color, selection_params, selection_color)`.
///
/// Shared by the compose overlay ([`sync_block_fx`]) and the editor surface
/// (`view::editor::render::sync_editor_cursor`) so the cursor beam/block/underline
/// shapes and the selection highlight render identically. `rtt_w`/`rtt_h` are the
/// surface's logical build dimensions (pixel rects → UV). Returns all-zero (no
/// composite) when there is no cursor to draw.
pub fn cursor_selection_uniforms(
    geom: &OverlayCursorGeometry,
    rtt_w: f32,
    rtt_h: f32,
    theme: &Theme,
) -> (Vec4, Vec4, Vec4, Vec4) {
    use crate::input::vim::CursorKind;
    if geom.height <= 0.0 || rtt_w <= 0.0 || rtt_h <= 0.0 {
        return (Vec4::ZERO, Vec4::ZERO, Vec4::ZERO, Vec4::ZERO);
    }

    // Block-cursor width: a fraction of line height. Mono fonts cluster around
    // ~0.55× height; close enough without re-querying parley for cluster advance.
    let block_width = (geom.height as f32 * 0.55).max(2.0);
    let glyph_h = geom.height as f32;
    let (rect_x, rect_y, rect_w, rect_h, color) = match geom.kind {
        CursorKind::Beam => (geom.x as f32, geom.y as f32, 2.0, glyph_h, theme.cursor_insert),
        CursorKind::Block => {
            (geom.x as f32, geom.y as f32, block_width, glyph_h, theme.cursor_normal)
        }
        CursorKind::Underline => {
            // Thin bar at the glyph baseline — vim Replace.
            let bar_h = (glyph_h * 0.12).max(2.0);
            let y = geom.y as f32 + glyph_h - bar_h;
            (geom.x as f32, y, block_width, bar_h, theme.cursor_replace)
        }
        // Hidden — selection rect renders instead. Zero rect skips the composite.
        CursorKind::Hidden => (0.0, 0.0, 0.0, 0.0, Vec4::ZERO),
    };

    let (cursor_params, cursor_color) = if rect_w > 0.0 && rect_h > 0.0 {
        (
            Vec4::new(rect_x / rtt_w, rect_y / rtt_h, rect_w / rtt_w, rect_h / rtt_h),
            Vec4::new(color.x, color.y, color.z, color.w),
        )
    } else {
        (Vec4::ZERO, Vec4::ZERO)
    };

    let (selection_params, selection_color) =
        if geom.selection_width > 0.0 && geom.selection_height > 0.0 {
            let s = theme.selection_bg.to_srgba();
            (
                Vec4::new(
                    geom.selection_x as f32 / rtt_w,
                    geom.selection_y as f32 / rtt_h,
                    geom.selection_width as f32 / rtt_w,
                    geom.selection_height as f32 / rtt_h,
                ),
                Vec4::new(s.red, s.green, s.blue, s.alpha),
            )
        } else {
            (Vec4::ZERO, Vec4::ZERO)
        };

    (cursor_params, cursor_color, selection_params, selection_color)
}
