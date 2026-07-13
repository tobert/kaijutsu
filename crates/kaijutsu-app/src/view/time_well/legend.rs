//! The transient keyboard legend — the last survivor of the retired edge HUD
//! (HUD melt, `docs/timewell.md`). Pressing `?` while dived into the well
//! toggles a corner readout of the well's verbs on/off; every other edge-HUD
//! panel (N/E/W/S) melted into scene-native surfaces in slices 1–3 and the
//! panel set itself retired in slice 4.
//!
//! Like the retired panels, this is an **in-scene MSDF panel** (the shared
//! [`super::panel`] primitive): a `Mesh3d` quad **parented to the well
//! camera**, so it rides the frame (screen-stable) yet lives in the scene —
//! HDR/bloom, depth, and the `WellCardMaterial` accent plate, same visual
//! language as the cards. Being a camera child it always faces the camera (no
//! billboard system).
//!
//! Unlike the old always-on `Legend` slot, this one is **transient**: spawned
//! on `?`, despawned on a second `?`, on zoom-out, or on room exit — the
//! well's mouth stays the open browser space until you ask for the cheat
//! sheet.

use std::f32::consts::FRAC_PI_4;

use bevy::prelude::*;

use super::panel::{commit_panel_glyphs, create_msdf_panel};
use crate::shaders::WellCardMaterial;
use crate::text::ShapingFonts;
use crate::text::components::bevy_color_to_brush;
use crate::text::msdf::{FontDataMap, MsdfAtlas, MsdfBlockGlyphs, collect_msdf_glyphs};
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};

/// Marker for the (at most one) live legend panel entity.
#[derive(Component)]
pub struct WellLegend;

// ── Layout tuning (all derived from the frustum; px values are texture-space). ──

/// Local distance in front of the camera the legend plane sits at. The panel
/// is a camera child, so this is constant in screen space and always renders
/// in front of every card (which live hundreds of units further down the
/// funnel).
const LEGEND_DEPTH: f32 = 100.0;
/// Gap from the frustum edge, as a fraction of the half-extent — the
/// size-aware fit in [`legend_transform`] keeps the panel's outer edge
/// exactly this far in, so a small value hugs the screen corner.
const LEGEND_MARGIN: f32 = 0.02;
/// Texture-space font size for the readout text.
const LEGEND_FONT_SIZE: f32 = 27.0;
/// Inner padding (texture px) — keeps the text inset from the frame so the
/// panel has breathing room inside its border.
const LEGEND_PAD: f32 = 30.0;
/// Border strength (the `WellCardMaterial.border` alpha) — no body fill, the
/// panel is a glowing frame, so it reads as a deliberate readout rather than
/// a stray card.
const LEGEND_BORDER_STRENGTH: f32 = 1.0;

/// Panel texture (logical authoring) size — wide enough for the two-column
/// "p promote      d demote" rows.
const LEGEND_TEX_W: f32 = 460.0;
const LEGEND_TEX_H: f32 = 260.0;

/// Static keyboard legend content — the verbs are provisional per the design;
/// this listing is their source of truth in-app. Laid out exactly once per
/// spawn (the legend never changes while it's up), so there's no per-frame
/// relayout to guard.
fn legend_text() -> String {
    "CONTROLS\n\
     p promote      d demote\n\
     z pause        a archive\n\
     c conclude     0-9 seat\n\
     \u{23ce} enter        esc back\n\
     ? legend"
        .to_string()
}

/// Read `(fov_y, aspect)` off a camera projection, falling back to sane
/// defaults for a non-perspective projection (the well always uses
/// perspective).
pub(crate) fn read_perspective(projection: &Projection) -> (f32, f32) {
    match projection {
        Projection::Perspective(p) => (p.fov, p.aspect_ratio),
        _ => (FRAC_PI_4, 16.0 / 9.0),
    }
}

/// The legend's child-local transform: bottom-left frustum-corner anchor +
/// a scale that sizes the shared unit quad. Recomputed on spawn and every
/// frame ([`position_legend`]) so it stays corner-locked across window
/// resizes.
fn legend_transform(fov_y: f32, aspect: f32) -> Transform {
    let half_h = LEGEND_DEPTH * (fov_y * 0.5).tan();
    let half_w = half_h * aspect;
    let inset = 1.0 - LEGEND_MARGIN;
    let anchor = Vec3::new(-half_w * inset, -half_h * inset, -LEGEND_DEPTH);

    let w = half_w * 0.32;
    let h = w / (LEGEND_TEX_W / LEGEND_TEX_H);
    // Pull IN from the corner anchor: right by half-width, UP by half-height
    // — dropping a bottom-anchored panel DOWN like a top-anchored one pushes
    // it half off-screen (the retired South HUD panel's live-caught bug,
    // 2026-07-04).
    let center = Vec3::new(anchor.x + w * 0.5, anchor.y + h * 0.5, anchor.z);
    Transform::from_translation(center).with_scale(Vec3::new(w, h, 1.0))
}

/// Toggle the legend on `?` (`KeyCode::Slash` — both `/` and `?` land on it,
/// Shift just changes the glyph). Dived-only, like the rest of the well's
/// keyboard handling.
pub fn toggle_legend(
    mut commands: Commands,
    keys: Res<ButtonInput<KeyCode>>,
    existing: Query<Entity, With<WellLegend>>,
    // `Without<FsnBackdropCamera>`: the backdrop's off-screen RTT camera is
    // ALSO a `Camera3d` (`view::fsn::backdrop`), and it's resident exactly
    // while `Screen::Room` is live — the same screen this toggle fires in.
    // Without the exclusion, two `Camera3d` entities would make `.single()`
    // fail the instant the backdrop spawns.
    camera: Query<(Entity, &Projection), (With<Camera3d>, Without<crate::view::fsn::backdrop::FsnBackdropCamera>)>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<WellCardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    palette: Res<crate::view::scene_palette::ScenePalette>,
) {
    if !keys.just_pressed(KeyCode::Slash) {
        return;
    }

    if let Ok(id) = existing.single() {
        commands.entity(id).despawn();
        return;
    }

    let Ok((cam_entity, projection)) = camera.single() else {
        return;
    };
    let Some(font) = fonts.get(&font_handles.mono) else {
        return; // font still loading; by dive time it always is
    };
    let Some(atlas) = atlas.as_deref_mut() else {
        return;
    };
    let (fov_y, aspect) = read_perspective(projection);

    let (image, panel) = create_msdf_panel(&mut images, LEGEND_TEX_W as u32, LEGEND_TEX_H as u32);
    let border = {
        let c = Color::srgb(0.95, 0.97, 1.0).to_linear();
        Vec4::new(
            c.red * palette.gain_hud_border,
            c.green * palette.gain_hud_border,
            c.blue * palette.gain_hud_border,
            LEGEND_BORDER_STRENGTH,
        )
    };
    let material = materials.add(WellCardMaterial {
        texture: image,
        // Black, near-opaque body: the panel interior deliberately blots out
        // the well behind it so the readout text stays legible.
        accent: Vec4::new(0.0, 0.0, 0.0, 0.94),
        params: Vec4::ZERO,
        shape: Vec4::new(LEGEND_TEX_W / LEGEND_TEX_H, 0.05, 0.018, 0.012),
        // Neutral readout white, selection-independent (unlike the retired
        // panels' selection-accent echo — the legend has no selection to echo).
        border,
        // dim.x = 1: never dimmed (not a rim Card). y/z are live
        // chatter/beat lanes — 0, or the frame washes cyan+gold.
        dim: Vec4::new(1.0, 0.0, 0.0, 0.0),
    });

    let layout = font.layout(
        &legend_text(),
        &VelloTextStyle { font_size: LEGEND_FONT_SIZE, line_height: 1.2, ..default() },
        VelloTextAlign::Left,
        Some(LEGEND_TEX_W - 2.0 * LEGEND_PAD),
    );
    for line in layout.lines() {
        for item in line.items() {
            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                font_data_map.register(gr.run().font());
            }
        }
    }
    let brush = bevy_color_to_brush(Color::srgb(0.95, 0.97, 1.0));
    let glyphs =
        collect_msdf_glyphs(&layout, &[], &brush, (LEGEND_PAD as f64, LEGEND_PAD as f64), atlas);

    let id = commands
        .spawn((
            WellLegend,
            Mesh3d(meshes.add(Rectangle::new(1.0, 1.0))),
            MeshMaterial3d(material),
            legend_transform(fov_y, aspect),
            Visibility::Inherited,
            panel,
            Name::new("WellLegend"),
        ))
        .insert(ChildOf(cam_entity))
        .id();
    let mut msdf = MsdfBlockGlyphs::default();
    commit_panel_glyphs(&mut msdf, glyphs);
    commands.entity(id).insert(msdf);
}

/// Keep the legend corner-locked: re-derive its transform from the live
/// projection (cheap — one `Vec3` + one scale) so it tracks window-aspect/FOV
/// changes.
pub fn position_legend(
    // Same `Without<FsnBackdropCamera>` exclusion as `toggle_legend` above —
    // this system runs every frame `Screen::Room` is live, exactly when the
    // backdrop's second `Camera3d` may also exist.
    camera: Query<&Projection, (With<Camera3d>, Without<crate::view::fsn::backdrop::FsnBackdropCamera>)>,
    mut legend: Query<&mut Transform, With<WellLegend>>,
) {
    let Ok(mut tf) = legend.single_mut() else {
        return;
    };
    let Ok(projection) = camera.single() else {
        return;
    };
    let (fov_y, aspect) = read_perspective(projection);
    *tf = legend_transform(fov_y, aspect);
}

/// Dismiss the legend on zoom-OUT (ambient tier, not dived-only — mirrors
/// `patch_bay::apply_patch_lod`'s own reasoning: a dived-only system freezes
/// whatever was live on the last dived frame instead of reacting to a
/// zoom-out transition). The legend is transient — surfacing dismisses it
/// rather than hiding it, so there's nothing to re-show on the next dive.
pub fn despawn_legend_unzoomed(
    room: Res<crate::view::room::RoomState>,
    legend: Query<Entity, With<WellLegend>>,
    mut commands: Commands,
) {
    if super::scene::well_zoomed(&room) {
        return;
    }
    for e in legend.iter() {
        commands.entity(e).despawn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legend_text_matches_the_documented_controls() {
        let text = legend_text();
        assert!(text.starts_with("CONTROLS"));
        for verb in [
            "p promote", "d demote", "z pause", "a archive", "c conclude", "0-9 seat", "esc back",
            "? legend",
        ] {
            assert!(text.contains(verb), "legend must mention {verb:?}: {text}");
        }
    }

    #[test]
    fn read_perspective_falls_back_for_non_perspective_projection() {
        let (fov, aspect) = read_perspective(&Projection::Orthographic(OrthographicProjection {
            ..OrthographicProjection::default_2d()
        }));
        assert_eq!(fov, FRAC_PI_4);
        assert_eq!(aspect, 16.0 / 9.0);
    }

    #[test]
    fn legend_sits_above_and_right_of_its_bottom_left_anchor() {
        let (fov, aspect) = (FRAC_PI_4, 16.0 / 9.0);
        let tf = legend_transform(fov, aspect);
        let half_h = LEGEND_DEPTH * (fov * 0.5).tan();
        let half_w = half_h * aspect;
        let anchor_x = -half_w * (1.0 - LEGEND_MARGIN);
        let anchor_y = -half_h * (1.0 - LEGEND_MARGIN);
        assert!(tf.translation.x > anchor_x, "legend center is right of its bottom-left anchor");
        assert!(tf.translation.y > anchor_y, "legend center is above its bottom-left anchor");
    }

    #[test]
    fn legend_stays_fully_on_screen() {
        let (fov, aspect) = (FRAC_PI_4, 16.0 / 9.0);
        let tf = legend_transform(fov, aspect);
        let half_h = LEGEND_DEPTH * (fov * 0.5).tan();
        let half_w = half_h * aspect;
        assert!(
            tf.translation.y - tf.scale.y * 0.5 >= -half_h,
            "legend's bottom edge stays inside the frustum"
        );
        assert!(
            tf.translation.x - tf.scale.x * 0.5 >= -half_w,
            "legend's left edge stays inside the frustum"
        );
    }

    #[test]
    fn legend_z_sits_at_the_fixed_depth() {
        let tf = legend_transform(FRAC_PI_4, 16.0 / 9.0);
        assert_eq!(tf.translation.z, -LEGEND_DEPTH);
    }

    #[test]
    fn wider_aspect_pushes_the_anchor_further_left() {
        let narrow = legend_transform(FRAC_PI_4, 1.0).translation.x;
        let wide = legend_transform(FRAC_PI_4, 2.0).translation.x;
        assert!(wide < narrow, "a wider aspect pushes the legend's left edge further out");
    }
}
