//! Editor panel rendering — the MSDF panel that draws a kernel editor session.
//!
//! Reuses the time-well's in-scene MSDF panel primitive
//! ([`create_msdf_panel`]/[`commit_panel_glyphs`]): one `Mesh3d` quad sampling an
//! RTT texture the MSDF pass rasterizes glyphs into. The panel sits at a fixed
//! depth in front of the app's always-on camera (which rests at identity for the
//! conversation, looking down −Z), so hiding the conversation chrome on
//! `OnEnter(Screen::Editor)` leaves the editor panel as the only thing in view —
//! no camera choreography. Text is laid out from [`ActiveEditor`]'s live state;
//! the cursor quad + selection rects are a follow-up (docs/vi.md).

use bevy::prelude::*;
use vello::peniko::Brush;

use super::ActiveEditor;
use crate::shaders::WellCardMaterial;
use crate::text::ShapingFonts;
use crate::text::components::bevy_color_to_brush;
use crate::text::msdf::{FontDataMap, MsdfAtlas, MsdfBlockGlyphs, collect_msdf_glyphs};
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};
use crate::view::time_well::panel::{commit_panel_glyphs, create_msdf_panel};
use crate::view::time_well::scene::card_shape;

/// RTT texture resolution (1.6 aspect, crisp at the panel's on-screen size).
const EDITOR_TEX_W: u32 = 1024;
const EDITOR_TEX_H: u32 = 640;
/// World-space quad size (1.6 aspect — matches `card_shape()`).
const EDITOR_QUAD_W: f32 = 460.0;
const EDITOR_QUAD_H: f32 = 287.5;
/// Depth in front of the camera (which looks down −Z from the origin). Chosen so
/// the quad fits inside the default perspective frustum (vertical FOV π/4) with
/// margin — half-height at this depth ≈ 157 > 144 (half the quad), so no clip.
const EDITOR_PANEL_Z: f32 = -380.0;
/// Inner padding in texture-space px.
const PAD: f32 = 26.0;

/// Marks the single editor MSDF panel (spawned on enter, despawned on exit).
#[derive(Component)]
pub struct EditorPanel;

/// Spawn the editor panel in front of the camera when entering `Screen::Editor`.
pub fn spawn_editor_panel(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut images: ResMut<Assets<Image>>,
    mut materials: ResMut<Assets<WellCardMaterial>>,
) {
    let mesh = meshes.add(Rectangle::new(EDITOR_QUAD_W, EDITOR_QUAD_H));
    let (image, panel) = create_msdf_panel(&mut images, EDITOR_TEX_W, EDITOR_TEX_H);
    let material = materials.add(WellCardMaterial {
        texture: image,
        // A dark editor "page" so text reads against a deliberate surface.
        accent: Vec4::new(0.07, 0.08, 0.11, 0.97),
        params: Vec4::ZERO,
        shape: card_shape(),
        border: Vec4::ZERO,
    });
    commands.spawn((
        EditorPanel,
        Mesh3d(mesh),
        MeshMaterial3d(material),
        Transform::from_translation(Vec3::new(0.0, 0.0, EDITOR_PANEL_Z)),
        Visibility::Inherited,
        // MSDF owns this texture (clears + renders text on transparent); the
        // shader draws the body. Pure MSDF — no vello/UiVectorScene.
        panel,
        Name::new("EditorPanel"),
    ));
}

/// Despawn the editor panel when leaving `Screen::Editor`.
pub fn despawn_editor_panel(mut commands: Commands, panels: Query<Entity, With<EditorPanel>>) {
    for e in panels.iter() {
        commands.entity(e).despawn();
    }
}

/// Lay out the active session's text into the panel's glyphs whenever the editor
/// state changes (open, keystroke echo, or a peer merge). Monospace, top-left.
pub fn render_editor_panel(
    active: Res<ActiveEditor>,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    mut panels: Query<&mut MsdfBlockGlyphs, With<EditorPanel>>,
) {
    // Re-rasterize only on a state change (the open seed + each push), not every
    // frame. `is_changed` is true on the first run after entering the screen.
    if !active.is_changed() {
        return;
    }
    let Some(view) = active.session.as_ref() else {
        return;
    };
    let Ok(mut msdf) = panels.single_mut() else {
        return;
    };
    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };
    let Some(atlas) = atlas.as_deref_mut() else {
        return;
    };

    let brush: Brush = bevy_color_to_brush(Color::srgb(0.90, 0.93, 0.98));
    let layout = font.layout(
        &view.state.text,
        &VelloTextStyle {
            font_size: 17.0,
            line_height: 1.3,
            ..default()
        },
        VelloTextAlign::Left,
        Some(EDITOR_TEX_W as f32 - 2.0 * PAD),
    );

    // Register each run's font with the atlas bridge (mirrors time-well's
    // `collect_field`), then collect the positioned MSDF glyphs.
    for line in layout.lines() {
        for item in line.items() {
            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                font_data_map.register(gr.run().font());
            }
        }
    }
    let glyphs = collect_msdf_glyphs(&layout, &[], &brush, (PAD as f64, PAD as f64), atlas);
    commit_panel_glyphs(&mut msdf, glyphs);
}
