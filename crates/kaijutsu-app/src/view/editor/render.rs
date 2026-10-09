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
//! Cursor geometry is computed here (parley) into [`OverlayCursorGeometry`],
//! and the visual-mode selection into [`EditorSelectionGeometry`]; both are
//! pushed to the material by [`sync_editor_cursor`].

use std::ops::Range;

use bevy::prelude::*;
use bevy::ui::ComputedNode;
use kaijutsu_types::editor::{EditorSelection, SelectionShape};

use super::ActiveEditor;
use crate::input::vim::mode_kind;
use crate::shaders::selection::coalesce_selection_rects;
use crate::shaders::{BlockFxMaterial, SelectionRect, pack_selection_rects};
use crate::text::msdf::{FontDataMap, MsdfAtlas, MsdfBlockGlyphs, collect_msdf_glyphs};
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
/// Top inset of the `:`-command strip from the page bottom, logical px — large
/// enough to clear the dock's bottom status/hint row (which the editor screen
/// still draws). Reserving a full status row for long docs is later polish.
const CMDLINE_BOTTOM_MARGIN: f32 = 72.0;

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

/// Convert the kernel's char spans to byte ranges into `text`, for parley.
/// Clamps to the end, like [`char_to_byte`].
fn selection_byte_ranges(text: &str, selection: &EditorSelection) -> Vec<Range<usize>> {
    selection
        .spans
        .iter()
        .map(|span| char_to_byte(text, span.start)..char_to_byte(text, span.end))
        .collect()
}

/// The rects a visual-mode selection covers in `layout`, in layout-local
/// logical px (add the text offset before packing).
///
/// A charwise or linewise selection is contiguous, so its per-row rects
/// coalesce to at most three, the middle band spanning `0..content_width`.
/// A blockwise selection keeps one rect per row: its rows are separate
/// column ranges, and a full-width band would cover columns outside the
/// block. More than [`crate::shaders::selection::MAX_SELECTION_RECTS`] blockwise rows
/// overflow the uniform, and `pack_selection_rects` drops the rest with a
/// warning.
pub(crate) fn editor_selection_rects<B: parley::Brush>(
    layout: &parley::Layout<B>,
    text: &str,
    selection: &EditorSelection,
    content_width: f32,
) -> Vec<SelectionRect> {
    let ranges = selection_byte_ranges(text, selection);
    match selection.shape {
        SelectionShape::Charwise | SelectionShape::Linewise => {
            let rows: Vec<SelectionRect> = ranges
                .into_iter()
                .flat_map(|bytes| crate::text::diff::layout_rects(layout, bytes))
                .collect();
            coalesce_selection_rects(&rows, 0.0, content_width)
        }
        SelectionShape::Blockwise => ranges
            .into_iter()
            .flat_map(|bytes| crate::text::diff::layout_rects(layout, bytes))
            .filter(|rect| rect.is_visible())
            .collect(),
    }
}

/// The editor surface's selection rects in surface-local logical px, built
/// with the glyphs and pushed to the material by [`sync_editor_cursor`].
/// Empty outside visual mode.
#[derive(Component, Default)]
pub struct EditorSelectionGeometry {
    pub rects: Vec<SelectionRect>,
    /// The selection the rects were built from, to detect a change.
    pub last: Option<EditorSelection>,
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
                crate::view::block_render::msdf_surface_bundle(material),
                OverlayCursorGeometry::default(),
                EditorSelectionGeometry::default(),
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
    // Last-rendered `:`-line, so a strip change (typing, submit) rebuilds the
    // glyphs even when the document text is unchanged.
    mut last_cmdline: Local<Option<String>>,
    mut surfaces: Query<
        (
            &mut BlockScene,
            &mut UiRttTexture,
            &mut MsdfBlockGlyphs,
            &ComputedNode,
            &mut OverlayCursorGeometry,
            &mut EditorSelectionGeometry,
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
    let Ok((mut scene, mut rtt, mut glyphs, computed, mut cursor_geom, mut selection_geom)) =
        surfaces.single_mut()
    else {
        return;
    };
    let Some(atlas) = atlas.as_deref_mut() else {
        return;
    };

    // ComputedNode is physical px; layout below is logical.
    let logical = crate::view::ui_rtt::logical_size(computed);
    let width = logical.x;
    let height = logical.y;
    if width <= 0.0 || height <= 0.0 {
        return;
    }

    let text = &view.state.text;
    let cursor_byte = char_to_byte(text, view.state.cursor as usize);
    let kind = mode_kind(view.state.mode.as_deref());
    // The bottom strip shows the in-progress `:`-line while typing, else a
    // transient status/error message (vim E492) after a bad `:`-submit. The
    // command line takes precedence — you're actively typing it.
    let cmdline = view
        .state
        .command_line
        .as_ref()
        .or(view.state.message.as_ref())
        .cloned();

    let size_changed =
        (rtt.built_width - width).abs() > 1.0 || (rtt.built_height - height).abs() > 1.0;
    let text_changed = scene.text != *text;
    let cursor_changed = cursor_geom.last_cursor_offset != cursor_byte;
    let kind_changed = cursor_geom.kind != kind;
    let cmdline_changed = last_cmdline.as_ref() != cmdline.as_ref();
    let selection_changed = selection_geom.last != view.state.selection;
    if !text_changed
        && !size_changed
        && !cursor_changed
        && !kind_changed
        && !cmdline_changed
        && !selection_changed
    {
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

    if text_changed || size_changed || cmdline_changed {
        for line in layout.lines() {
            for item in line.items() {
                if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                    font_data_map.register(gr.run().font());
                }
            }
        }
        let mut g = collect_msdf_glyphs(&layout, &[], &style.brush, text_offset, atlas);

        // The `:`-command strip (Slice 3): laid out with the same mono font and
        // appended to the same glyph buffer near the page bottom — so the editor
        // draws its command line read-only, no second surface, no mode tracking.
        // Also carries the status/error message (vim E492) after a bad submit.
        if let Some(cl) = &cmdline {
            let strip_layout = font.layout(cl, &style, VelloTextAlign::Left, Some(content_width));
            for line in strip_layout.lines() {
                for item in line.items() {
                    if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                        font_data_map.register(gr.run().font());
                    }
                }
            }
            let strip_offset = (PAD as f64, (height - CMDLINE_BOTTOM_MARGIN) as f64);
            let mut strip = collect_msdf_glyphs(&strip_layout, &[], &style.brush, strip_offset, atlas);
            g.append(&mut strip);
        }

        glyphs.glyphs = g;
        glyphs.version = glyphs.version.wrapping_add(1);

        rtt.built_width = width;
        rtt.built_height = height;
        scene.text = text.clone();
        scene.color = text_color;
        scene.content_version = scene.content_version.wrapping_add(1);
        scene.last_built_version = scene.content_version;
        *last_cmdline = cmdline.clone();
    }

    // Selection rects, moved from layout space onto the surface.
    selection_geom.rects = match &view.state.selection {
        Some(selection) => editor_selection_rects(&layout, text, selection, content_width)
            .into_iter()
            .map(|r| SelectionRect::new(r.x + PAD, r.y + TOP_MARGIN, r.width, r.height))
            .collect(),
        None => Vec::new(),
    };
    selection_geom.last = view.state.selection.clone();

    // Cursor geometry (pushed to the material by sync_editor_cursor).
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
/// math the compose overlay uses), and its visual-mode selection rects into the
/// selection uniform. The editor cursor is shown outside visual mode — the
/// surface only exists on `Screen::Editor`, which always owns the keyboard.
pub fn sync_editor_cursor(
    surfaces: Query<
        (
            &MaterialNode<BlockFxMaterial>,
            &OverlayCursorGeometry,
            &EditorSelectionGeometry,
            &UiRttTexture,
        ),
        With<EditorSurface>,
    >,
    mut materials: ResMut<Assets<BlockFxMaterial>>,
    theme: Res<Theme>,
) {
    for (mat_node, geom, selection, rtt) in surfaces.iter() {
        let Some(mut mat) = materials.get_mut(&mat_node.0) else {
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
        if selection.rects.is_empty() {
            mat.selection_rects = sp;
            mat.selection_color = sc;
        } else {
            let bg = theme.selection_bg.to_srgba();
            mat.selection_rects =
                pack_selection_rects(&selection.rects, rtt.built_width, rtt.built_height);
            mat.selection_color = Vec4::new(bg.red, bg.green, bg.blue, bg.alpha);
        }
    }
}

#[cfg(test)]
#[allow(clippy::single_range_in_vec_init)] // a span list holding one `Range` is the intended value
mod tests {
    use super::*;

    /// The shipped mono font, registered as the asset loader does: the rect
    /// math is parley's, so the test shapes for real.
    fn laid_out(text: &str, width: Option<f32>) -> parley::Layout<peniko::Brush> {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/fonts/NotoMono-Regular.ttf"
        ))
        .expect("shipped test font must be present");
        crate::text::shaping::load_into_font_context(bytes).layout(
            text,
            &VelloTextStyle {
                font_size: 16.0,
                ..Default::default()
            },
            VelloTextAlign::Left,
            width,
        )
    }

    fn selection(shape: SelectionShape, spans: Vec<Range<usize>>) -> EditorSelection {
        EditorSelection { shape, spans }
    }

    #[test]
    fn char_spans_become_byte_ranges_past_multibyte_chars() {
        // c a f é(2 bytes) \n x
        let text = "café\nx";
        let sel = selection(SelectionShape::Charwise, vec![3..6]);
        assert_eq!(selection_byte_ranges(text, &sel), vec![3..7]);
        let past_end = selection(SelectionShape::Charwise, vec![5..99]);
        assert_eq!(selection_byte_ranges(text, &past_end), vec![6..7], "clamped to the text end");
    }

    #[test]
    fn a_charwise_selection_inside_a_row_is_one_rect_at_its_text() {
        let text = "hello world";
        let layout = laid_out(text, None);
        let rects = editor_selection_rects(&layout, text, &selection(SelectionShape::Charwise, vec![6..11]), 400.0);
        assert_eq!(rects.len(), 1);
        assert!(rects[0].x > 0.0, "it starts at `world`, not at the margin");
        assert!(rects[0].right() < 400.0, "it ends at the text, not at the content edge");
    }

    /// Across four rows a contiguous selection is head, band, tail; the head
    /// runs to the content edge because its line break is selected.
    #[test]
    fn a_contiguous_selection_across_rows_coalesces_to_three_rects() {
        let text = "aaaa\nbbbb\ncccc\ndddd";
        let layout = laid_out(text, None);
        let rects = editor_selection_rects(&layout, text, &selection(SelectionShape::Charwise, vec![2..17]), 400.0);
        assert_eq!(rects.len(), 3, "{rects:?}");
        assert_eq!(rects[0].right(), 400.0, "the head reaches the content edge");
        assert_eq!((rects[1].x, rects[1].right()), (0.0, 400.0), "the band is full width");
        assert_eq!(rects[2].x, 0.0, "the tail starts at the margin");
        assert!(rects[2].right() < 400.0, "the tail stops at its last selected char");
    }

    #[test]
    fn a_blockwise_selection_keeps_one_rect_per_row() {
        let text = "abcd\nx\nefgh";
        let layout = laid_out(text, None);
        let rects = editor_selection_rects(
            &layout,
            text,
            &selection(SelectionShape::Blockwise, vec![1..3, 8..10]),
            400.0,
        );
        assert_eq!(rects.len(), 2, "{rects:?}");
        assert_eq!(rects[0].x, rects[1].x, "both rows start at the block's left column");
        assert!(rects[0].x > 0.0 && rects[0].right() < 400.0, "no row widens to the content edge");
        assert!(rects[0].y < rects[1].y);
    }

    /// A linewise selection of an empty line still covers something, so the
    /// reader can see which line is selected.
    #[test]
    fn a_selected_empty_line_is_visible() {
        let text = "a\n\nb";
        let layout = laid_out(text, None);
        let rects = editor_selection_rects(&layout, text, &selection(SelectionShape::Linewise, vec![2..3]), 400.0);
        assert!(rects.iter().any(|r| r.is_visible()), "{rects:?}");
    }
}
