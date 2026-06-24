//! Editor surface rendering — the editor's **own** 2D full-screen MSDF surface.
//!
//! The editor deserves its own specializations (full-screen, conversation-matched
//! font, a real text cursor, multi-line selection), so rather than reuse the
//! time-well's 3D card it gets a dedicated UI surface: a full-window node with a
//! dark "page" background and an MSDF text child fed by [`BlockFxMaterial`] — the
//! same material the conversation/compose surfaces use, so the cursor/selection
//! shader path is shared (see [`crate::shaders::cursor_selection_uniforms`]).
//!
//! Text is laid out from [`ActiveEditor`] at the conversation's `cell_font_size`.
//! Cursor geometry is computed here (parley) into [`OverlayCursorGeometry`] and
//! pushed to the material by [`sync_editor_cursor`].

use bevy::prelude::*;
use bevy::ui::ComputedNode;
use bevy::ui::widget::ImageNode;

use super::ActiveEditor;
use crate::input::vim::mode_kind;
use crate::shaders::BlockFxMaterial;
use crate::text::msdf::{BlockRenderMethod, FontDataMap, MsdfAtlas, MsdfBlockGlyphs, collect_msdf_glyphs};
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};
use crate::text::{ShapingFonts, TextMetrics, bevy_color_to_brush};
use crate::ui::theme::Theme;
use crate::view::block_render::BlockScene;
use crate::view::components::OverlayCursorGeometry;
use crate::view::ui_rtt::UiRttTexture;

/// Horizontal text inset from the surface edge, logical px.
const PAD: f32 = 28.0;
/// Top inset, logical px — larger than `PAD` so the first line clears the
/// top-left "会術 Kaijutsu" HUD title (which renders above the editor page).
const TOP_MARGIN: f32 = 52.0;

/// Full-window root that paints the dark editor page behind the text child.
#[derive(Component)]
pub struct EditorSurfaceRoot;

/// The MSDF text child: holds the glyphs, RTT, material, and cursor geometry.
#[derive(Component)]
pub struct EditorSurface;

/// Convert a char offset (the kernel's `EditorState.cursor`) to a byte offset for
/// parley. Clamps to the end.
fn char_to_byte(s: &str, char_off: usize) -> usize {
    s.char_indices()
        .nth(char_off)
        .map(|(b, _)| b)
        .unwrap_or(s.len())
}

/// Spawn the editor surface on entering `Screen::Editor`: a full-window page node
/// with one MSDF text child.
pub fn spawn_editor_panel(
    mut commands: Commands,
    mut fx_materials: ResMut<Assets<BlockFxMaterial>>,
) {
    let material = fx_materials.add(BlockFxMaterial::default());
    // A deliberate dark "page" so text reads against a real surface.
    let page = Color::srgb(0.07, 0.08, 0.11);
    commands
        .spawn((
            EditorSurfaceRoot,
            Node {
                position_type: PositionType::Absolute,
                top: Val::Px(0.0),
                left: Val::Px(0.0),
                width: Val::Percent(100.0),
                height: Val::Percent(100.0),
                ..default()
            },
            BackgroundColor(page),
            ZIndex(crate::constants::ZLayer::MODAL),
            Visibility::Inherited,
            Name::new("EditorSurfaceRoot"),
        ))
        .with_children(|parent| {
            parent.spawn((
                EditorSurface,
                BlockScene::default(),
                UiRttTexture::default(),
                MsdfBlockGlyphs::default(),
                BlockRenderMethod::Msdf,
                ImageNode::default(),
                MaterialNode(material),
                OverlayCursorGeometry::default(),
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
                Name::new("EditorSurface"),
            ));
        });
}

/// Despawn the surface (and its child) when leaving `Screen::Editor`.
pub fn despawn_editor_panel(mut commands: Commands, roots: Query<Entity, With<EditorSurfaceRoot>>) {
    for e in roots.iter() {
        commands.entity(e).despawn();
    }
}

/// Lay out the active session's text into the surface's MSDF glyphs and compute
/// the cursor geometry. Runs in PostUpdate after `UiSystems::Layout` so the
/// surface's `ComputedNode` (full-window size) is available. Rebuilds glyphs only
/// on a text/size change; recomputes cursor geometry on any change.
pub fn build_editor_surface(
    active: Res<ActiveEditor>,
    mut surfaces: Query<
        (
            &mut BlockScene,
            &mut UiRttTexture,
            &mut MsdfBlockGlyphs,
            &ComputedNode,
            &mut OverlayCursorGeometry,
        ),
        With<EditorSurface>,
    >,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    text_metrics: Res<TextMetrics>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
) {
    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };
    let Some(view) = active.session.as_ref() else {
        return;
    };
    let Ok((mut scene, mut rtt, mut glyphs, computed, mut cursor_geom)) = surfaces.single_mut()
    else {
        return;
    };
    let Some(atlas) = atlas.as_deref_mut() else {
        return;
    };

    let width = computed.size().x;
    let height = computed.size().y;
    if width <= 0.0 || height <= 0.0 {
        return;
    }

    let text = &view.state.text;
    let cursor_byte = char_to_byte(text, view.state.cursor as usize);
    let kind = mode_kind(view.state.mode.as_deref());

    let size_changed =
        (rtt.built_width - width).abs() > 1.0 || (rtt.built_height - height).abs() > 1.0;
    let text_changed = scene.text != *text;
    let cursor_changed = cursor_geom.last_cursor_offset != cursor_byte;
    let kind_changed = cursor_geom.kind != kind;
    if !text_changed && !size_changed && !cursor_changed && !kind_changed {
        return;
    }

    // Light text on the dark page.
    let text_color = Color::srgb(0.90, 0.93, 0.98);
    let brush = bevy_color_to_brush(text_color);
    let content_width = (width - 2.0 * PAD).max(0.0);
    let style = VelloTextStyle {
        brush,
        font_size: text_metrics.cell_font_size,
        ..default()
    };
    let layout = font.layout(text, &style, VelloTextAlign::Left, Some(content_width));

    let text_offset = (PAD as f64, TOP_MARGIN as f64);

    if text_changed || size_changed {
        for line in layout.lines() {
            for item in line.items() {
                if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                    font_data_map.register(gr.run().font());
                }
            }
        }
        let g = collect_msdf_glyphs(&layout, &[], &style.brush, text_offset, atlas);
        glyphs.glyphs = g;
        glyphs.version = glyphs.version.wrapping_add(1);

        rtt.built_width = width;
        rtt.built_height = height;
        scene.text = text.clone();
        scene.color = text_color;
        scene.content_version = scene.content_version.wrapping_add(1);
        scene.last_built_version = scene.content_version;
        scene.scene_version = scene.scene_version.wrapping_add(1);
    }

    // Cursor geometry (pushed to the material by sync_editor_cursor). Multi-line
    // selection — the editor specialization — is wired in a later slice.
    let cursor = parley::editing::Cursor::from_byte_index(
        &layout,
        cursor_byte,
        parley::layout::Affinity::Upstream,
    );
    let geom = cursor.geometry(&layout, 2.0);
    cursor_geom.x = text_offset.0 + geom.x0;
    cursor_geom.y = text_offset.1 + geom.y0;
    cursor_geom.height = geom.y1 - geom.y0;
    cursor_geom.last_cursor_offset = cursor_byte;
    cursor_geom.kind = kind;
}

/// Push the surface's cursor geometry into its `BlockFxMaterial` cursor uniform,
/// via the shared [`crate::shaders::cursor_selection_uniforms`] helper (the same
/// math the compose overlay uses). The editor cursor is always shown — the
/// surface only exists on `Screen::Editor`, which always owns the keyboard.
pub fn sync_editor_cursor(
    surfaces: Query<
        (&MaterialNode<BlockFxMaterial>, &OverlayCursorGeometry, &UiRttTexture),
        With<EditorSurface>,
    >,
    mut materials: ResMut<Assets<BlockFxMaterial>>,
    theme: Res<Theme>,
) {
    for (mat_node, geom, rtt) in surfaces.iter() {
        let Some(mat) = materials.get_mut(&mat_node.0) else {
            continue;
        };
        let (cp, cc, sp, sc) = crate::shaders::cursor_selection_uniforms(
            geom,
            rtt.built_width,
            rtt.built_height,
            &theme,
        );
        mat.cursor_params = cp;
        mat.cursor_color = cc;
        mat.selection_params = sp;
        mat.selection_color = sc;
    }
}
