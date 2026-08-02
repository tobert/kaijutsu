//! The diff viewer's own full-screen MSDF surface.
//!
//! The editor's surface (`view::editor::render`) with two additions the diff
//! needs and the editor does not:
//!
//! - **Semantic color and background bands.** The same pair the inline preview
//!   uses — `SpanBrush` per line for the text, `MsdfBlockGeometry` quads for
//!   the `+`/`-` bands — so the two surfaces cannot drift on what a diff looks
//!   like. Visual-line selection is one more band layer, which is exactly why
//!   the viewer's visual mode is line-wise until the multi-rect
//!   `BlockFxMaterial` extension lands.
//! - **A row window.** A diff bounded only by `MAX_RENDER_BYTES` is up to a
//!   megabyte; laying all of it out to draw one screenful would stall the
//!   frame. The surface lays out a band of rows around the cursor and slides
//!   it when the cursor leaves — the diff equivalent of the conversation's
//!   spawn band.
//!
//! HiDPI: `ComputedNode` is **physical** px and everything below is logical,
//! so the size goes through `view::ui_rtt::logical_size` exactly once, at the
//! top.

use bevy::prelude::*;
use bevy::ui::ComputedNode;
use bevy::ui::widget::ImageNode;

use super::{ActiveDiffView, DiffViewContent};
use crate::shaders::BlockFxMaterial;
use crate::text::msdf::{
    BlockRenderMethod, FontDataMap, MsdfAtlas, MsdfBlockGeometry, MsdfBlockGlyphs,
    collect_msdf_glyphs,
};
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};
use crate::text::{ShapingFonts, TextMetrics, bevy_color_to_brush};
use crate::ui::theme::Theme;
use crate::view::block_render::BlockScene;
use crate::view::components::OverlayCursorGeometry;
use crate::view::ui_rtt::UiRttTexture;

/// Horizontal text inset from the surface edge, logical px.
const PAD: f32 = 28.0;
/// Top inset — larger than `PAD` so the first line clears the top-left
/// "会術 Kaijutsu" HUD title, which draws above this page.
const TOP_MARGIN: f32 = 52.0;
/// Inset of the status strip from the page bottom, logical px — enough to
/// clear the dock's own bottom row.
const STATUS_BOTTOM_MARGIN: f32 = 72.0;
/// Extra rows laid out past the visible band, so a one-line cursor move does
/// not force a relayout and a partially visible last row still draws.
const WINDOW_SLACK_ROWS: usize = 24;

/// Full-window root painting the dark page behind the text child.
#[derive(Component)]
pub struct DiffSurfaceRoot;

/// The MSDF text child: glyphs, band geometry, RTT, material, cursor.
#[derive(Component)]
pub struct DiffSurface;

/// Which rows the surface most recently laid out, and what it laid them out
/// *for*. Everything here is compared each frame to decide whether to rebuild
/// — a diff viewer is idle almost all the time.
#[derive(Component, Default)]
pub struct DiffSurfaceWindow {
    /// First core row in the laid-out band.
    first_row: usize,
    /// One past the last.
    end_row: usize,
    /// State the last build was for.
    built: Option<BuildKey>,
}

/// The inputs a rebuild depends on. A change in any of them redraws.
#[derive(PartialEq, Eq, Clone, Debug)]
struct BuildKey {
    content_hash: u64,
    first_row: usize,
    end_row: usize,
    cursor_row: usize,
    selection: Option<(usize, usize)>,
    stale: bool,
    status: String,
    width_px: i32,
    height_px: i32,
}

/// Spawn the surface on entering `Screen::Diff`.
pub fn spawn_diff_panel(mut commands: Commands, mut fx_materials: ResMut<Assets<BlockFxMaterial>>) {
    let material = fx_materials.add(BlockFxMaterial::default());
    // The editor's page color: this is the same kind of surface, and two
    // different "dark pages" would read as a bug.
    let page = Color::srgb(0.07, 0.08, 0.11);
    commands
        .spawn((
            DiffSurfaceRoot,
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
            Name::new("DiffSurfaceRoot"),
        ))
        .with_children(|parent| {
            parent.spawn((
                DiffSurface,
                DiffSurfaceWindow::default(),
                BlockScene::default(),
                UiRttTexture::default(),
                MsdfBlockGlyphs::default(),
                MsdfBlockGeometry::default(),
                BlockRenderMethod::Msdf,
                ImageNode::default(),
                MaterialNode(material),
                OverlayCursorGeometry::default(),
                Node {
                    width: Val::Percent(100.0),
                    height: Val::Percent(100.0),
                    ..default()
                },
                Name::new("DiffSurface"),
            ));
        });
}

/// Despawn the surface when leaving `Screen::Diff`.
pub fn despawn_diff_panel(mut commands: Commands, roots: Query<Entity, With<DiffSurfaceRoot>>) {
    for e in roots.iter() {
        commands.entity(e).despawn();
    }
}

/// Slide the row window so `cursor` is inside it, returning the new bounds.
///
/// Pure so the scroll rule is testable without a schedule: the window only
/// moves when the cursor leaves it, which is what makes small motions free.
fn window_for(
    cursor: usize,
    visible: usize,
    total: usize,
    current: (usize, usize),
) -> (usize, usize) {
    let visible = visible.max(1);
    let (mut first, _) = current;
    if cursor < first {
        first = cursor;
    } else if cursor >= first + visible {
        first = cursor + 1 - visible;
    }
    first = first.min(total.saturating_sub(1));
    (first, (first + visible + WINDOW_SLACK_ROWS).min(total))
}

/// Lay the active session out into glyphs, bands, and cursor geometry.
#[allow(clippy::too_many_arguments)]
pub fn build_diff_surface(
    active: Res<ActiveDiffView>,
    mut surfaces: Query<
        (
            &mut BlockScene,
            &mut UiRttTexture,
            &mut MsdfBlockGlyphs,
            &mut MsdfBlockGeometry,
            &mut DiffSurfaceWindow,
            &ComputedNode,
            &mut OverlayCursorGeometry,
        ),
        With<DiffSurface>,
    >,
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    text_metrics: Res<TextMetrics>,
    theme: Res<Theme>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
) {
    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };
    let Some(session) = active.session.as_ref() else {
        return;
    };
    let Ok((mut scene, mut rtt, mut glyphs, mut geometry, mut window, computed, mut cursor_geom)) =
        surfaces.single_mut()
    else {
        return;
    };
    let Some(atlas) = atlas.as_deref_mut() else {
        return;
    };

    // ComputedNode is PHYSICAL px; everything below is logical.
    let logical = crate::view::ui_rtt::logical_size(computed);
    let (width, height) = (logical.x, logical.y);
    if width <= 0.0 || height <= 0.0 {
        return;
    }

    let content_width = (width - 2.0 * PAD).max(0.0);
    let line_height = text_metrics.cell_font_size.max(1.0) * 1.3;
    let visible_rows = (((height - TOP_MARGIN - STATUS_BOTTOM_MARGIN) / line_height).floor() as i64)
        .max(1) as usize;

    // ── what to draw ────────────────────────────────────────────────────────
    let (preview, first_row, cursor_row, selection) = match &session.content {
        DiffViewContent::Ready(core) => {
            let cursor = core.cursor_row();
            let selection = core.selection_rows();
            let total = core.rows().len();
            let (first, end) = window_for(
                cursor,
                visible_rows,
                total,
                (window.first_row, window.end_row),
            );
            let preview = crate::text::diff::view_rows(core.as_ref(), first..end);
            (preview, first, cursor, selection)
        }
        DiffViewContent::Failed { reason, source } => {
            (crate::text::diff::error_view(source, reason), 0, 0, None)
        }
    };

    let status = status_line(session, cursor_row);
    let key = BuildKey {
        content_hash: session.content_hash,
        first_row,
        end_row: first_row + preview.lines.len(),
        cursor_row,
        selection,
        stale: session.stale,
        status: status.clone(),
        width_px: width as i32,
        height_px: height as i32,
    };
    if window.built.as_ref() == Some(&key) {
        return;
    }

    // ── layout ──────────────────────────────────────────────────────────────
    let fallback = bevy_color_to_brush(theme.diff_context_fg);
    let style = VelloTextStyle {
        brush: fallback.clone(),
        font_size: text_metrics.cell_font_size,
        ..default()
    };
    let layout = font.layout(
        &preview.plain_text,
        &style,
        VelloTextAlign::Left,
        Some(content_width),
    );
    let text_offset = (PAD as f64, TOP_MARGIN as f64);

    for line in layout.lines() {
        for item in line.items() {
            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                font_data_map.register(gr.run().font());
            }
        }
    }

    let span_brushes = crate::text::rich::build_diff_span_brushes(&preview, &theme);
    let mut g = collect_msdf_glyphs(&layout, &span_brushes, &fallback, text_offset, atlas);

    // The status strip, laid out with the same font and appended to the same
    // glyph buffer — one surface, no second material.
    let strip_style = VelloTextStyle {
        brush: bevy_color_to_brush(if session.stale {
            theme.diff_error_fg
        } else {
            theme.diff_meta
        }),
        font_size: text_metrics.cell_font_size,
        ..default()
    };
    let strip_layout = font.layout(
        &status,
        &strip_style,
        VelloTextAlign::Left,
        Some(content_width),
    );
    for line in strip_layout.lines() {
        for item in line.items() {
            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                font_data_map.register(gr.run().font());
            }
        }
    }
    let strip_offset = (PAD as f64, (height - STATUS_BOTTOM_MARGIN) as f64);
    let mut strip =
        collect_msdf_glyphs(&strip_layout, &[], &strip_style.brush, strip_offset, atlas);
    g.append(&mut strip);

    // ── bands ───────────────────────────────────────────────────────────────
    // One entry per WRAPPED visual row, addressed by its start byte — a long
    // `+` line must be tinted across every row it occupies.
    let rows: Vec<crate::text::diff::PreviewRow> = layout
        .lines()
        .map(|line| {
            let m = line.metrics();
            crate::text::diff::PreviewRow {
                text_offset: line.text_range().start,
                top: m.min_coord,
                bottom: m.max_coord,
            }
        })
        .collect();

    let offset = (PAD, TOP_MARGIN);
    let mut verts = crate::text::diff::build_diff_band_geometry(
        &preview,
        &rows,
        content_width,
        offset,
        &crate::text::rich::diff_band_colors(&theme),
    );
    // Selection rides ON TOP of the +/- bands: a selected insertion should
    // read as both. Row indices are window-relative.
    if let Some((a, b)) = selection {
        let local = (a.saturating_sub(first_row), b.saturating_sub(first_row));
        if b >= first_row {
            verts.extend(crate::text::diff::build_selection_band_geometry(
                &preview,
                &rows,
                local,
                content_width,
                offset,
                rgba8(theme.selection_bg),
            ));
        }
    }

    // Geometry and glyphs rebuild together under the single shared version
    // gate (`MsdfBlockGeometry`'s contract: fill both, bump once).
    geometry.vertices = verts;
    glyphs.glyphs = g;
    glyphs.version = glyphs.version.wrapping_add(1);

    rtt.built_width = width;
    rtt.built_height = height;
    scene.text = preview.plain_text.clone();
    scene.content_version = scene.content_version.wrapping_add(1);
    scene.last_built_version = scene.content_version;
    scene.scene_version = scene.scene_version.wrapping_add(1);

    window.first_row = first_row;
    window.end_row = key.end_row;
    window.built = Some(key);

    // ── cursor ──────────────────────────────────────────────────────────────
    // The cursor sits at the start of its row, which is where a line-wise
    // viewer's cursor belongs: there is no column to preserve.
    let byte = preview
        .lines
        .get(cursor_row.saturating_sub(first_row))
        .map(|l| l.start)
        .unwrap_or(0);
    let cursor = parley::editing::Cursor::from_byte_index(
        &layout,
        byte,
        parley::layout::Affinity::Downstream,
    );
    let geom = cursor.geometry(&layout, 2.0);
    cursor_geom.x = text_offset.0 + geom.x0;
    cursor_geom.y = text_offset.1 + geom.y0;
    cursor_geom.height = geom.y1 - geom.y0;
    cursor_geom.last_cursor_offset = byte;
}

/// RGBA8 for the geometry vertex format.
fn rgba8(color: Color) -> [u8; 4] {
    let c = color.to_srgba();
    [
        (c.red.clamp(0.0, 1.0) * 255.0) as u8,
        (c.green.clamp(0.0, 1.0) * 255.0) as u8,
        (c.blue.clamp(0.0, 1.0) * 255.0) as u8,
        (c.alpha.clamp(0.0, 1.0) * 255.0) as u8,
    ]
}

/// The bottom strip: what you are reading, where you are, and — loudly — that
/// the block has changed underneath you.
///
/// The stale banner is the visible half of freeze-on-open. It names the key
/// that rebinds, because a banner that only says "stale" leaves the reader
/// with no move to make.
fn status_line(session: &super::DiffViewSession, cursor_row: usize) -> String {
    let mut s = session.title.clone();
    match &session.content {
        DiffViewContent::Ready(core) => {
            s.push_str(&format!("  {}", core.model().stat()));
            if !core.model().is_complete() {
                s.push_str("  (truncated for display)");
            }
            s.push_str(&format!(
                "  {}/{}",
                cursor_row + 1,
                core.rows().len().max(1)
            ));
            if let Some(mode) = core.mode() {
                s.push_str(&format!("  {mode}"));
            }
            if let Some(cmd) = core.command_line() {
                s.push_str(&format!("  {cmd}"));
            }
        }
        DiffViewContent::Failed { .. } => {
            s.push_str("  [does not parse — q to close]");
        }
    }
    if session.stale {
        s.push_str("  ⚠ BLOCK CHANGED — R to reload, or keep reading the frozen copy");
    }
    s
}

/// Push the surface's cursor geometry into its material, via the shared
/// helper the compose overlay and editor use.
pub fn sync_diff_cursor(
    surfaces: Query<
        (
            &MaterialNode<BlockFxMaterial>,
            &OverlayCursorGeometry,
            &UiRttTexture,
        ),
        With<DiffSurface>,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The window only moves when the cursor leaves it — that is what makes
    /// `j` free on a hundred-thousand-line diff.
    #[test]
    fn the_window_holds_still_while_the_cursor_is_inside_it() {
        let (first, end) = window_for(10, 40, 1_000, (0, 40));
        assert_eq!(first, 0);
        assert_eq!(end, 40 + WINDOW_SLACK_ROWS);
        assert_eq!(window_for(39, 40, 1_000, (0, 64)).0, 0);
    }

    #[test]
    fn the_window_follows_the_cursor_down_by_the_minimum() {
        // Cursor one past the bottom: the band scrolls exactly one row.
        let (first, _) = window_for(40, 40, 1_000, (0, 64));
        assert_eq!(first, 1);
        // A jump lands the cursor on the last visible row, not the first.
        let (first, _) = window_for(500, 40, 1_000, (0, 64));
        assert_eq!(first, 461);
    }

    #[test]
    fn the_window_follows_the_cursor_up_to_the_top_row() {
        let (first, _) = window_for(100, 40, 1_000, (200, 264));
        assert_eq!(first, 100, "scrolling up puts the cursor at the top");
        let (first, _) = window_for(0, 40, 1_000, (200, 264));
        assert_eq!(first, 0);
    }

    #[test]
    fn the_window_clamps_to_a_short_or_empty_document() {
        let (first, end) = window_for(0, 40, 5, (0, 0));
        assert_eq!((first, end), (0, 5), "never past the end");
        let (first, end) = window_for(0, 40, 0, (0, 0));
        assert_eq!((first, end), (0, 0), "an empty diff draws nothing");
        // A degenerate viewport still lays out at least one row.
        let (_, end) = window_for(0, 0, 10, (0, 0));
        assert!(end >= 1);
    }
}
