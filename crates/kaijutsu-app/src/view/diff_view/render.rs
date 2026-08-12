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

use super::{ActiveDiffView, DiffViewContent};
use crate::input::vim::{CursorKind, mode_kind};
use crate::shaders::BlockFxMaterial;
use crate::text::msdf::{
    FontDataMap, MsdfAtlas, MsdfBlockGeometry, MsdfBlockGlyphs,
    collect_msdf_glyphs, collect_msdf_glyphs_styled,
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
/// Extra rows laid out past the visible band, so a partially visible last row
/// still draws and a wrapped line near the bottom edge is not cut mid-line.
///
/// It does **not** widen the range the cursor may roam before the band slides:
/// the cursor has to stay on screen, and the surface draws from the top with
/// no scroll offset, so `viewport_rows` is the real bound. Motion *inside* the
/// band is free (see `DiffSurfaceWindow`); walking off the bottom edge still
/// re-shapes one band per row, which is what a viewport without a scroll
/// offset costs. Cheap to improve later — the `Layout` is cached now (the
/// cursor needs it), so this wants moving the glyph offset instead of
/// re-collecting — but not worth the complexity until a profile says so.
const WINDOW_SLACK_ROWS: usize = 24;
/// Width of the minimap strip down the right edge, logical px.
const MINIMAP_WIDTH: f32 = 12.0;
/// Gap between the text column and the strip, logical px.
const MINIMAP_GAP: f32 = 12.0;
/// Height of one minimap slot, logical px.
///
/// Not one slot per pixel: a taller slot is a shorter bucket list to build and
/// to memcpy, and at three pixels a hunk boundary is still a legible tick. The
/// strip's resolution is a compression choice, not a fidelity one — the
/// counts inside a bucket are what keep a lone change visible
/// (`kaijutsu_diff::minimap`).
const MINIMAP_SLOT_PX: f32 = 3.0;

/// Full-window root painting the dark page behind the text child.
#[derive(Component)]
pub struct DiffSurfaceRoot;

/// The MSDF text child: glyphs, band geometry, RTT, material, cursor.
#[derive(Component)]
pub struct DiffSurface;

/// What the surface has laid out, and what it drew on top of it.
///
/// **Two keys, deliberately.** Shaping the window's text is the expensive
/// half; moving the cursor within it must not pay for that. [`LayoutKey`]
/// gates the shaping — it changes only when the band, the content, or the
/// viewport does — while [`CursorKey`] gates the cheap pass (status strip,
/// selection bands, cursor geometry) that runs on top of the cached layout.
/// A `j` inside the window is a cheap pass, which is the entire point of
/// having a window at all.
#[derive(Component, Default)]
pub struct DiffSurfaceWindow {
    /// First core row in the laid-out band.
    first_row: usize,
    /// One past the last.
    end_row: usize,
    /// What the cached layout was shaped for; `None` before the first build.
    laid_out: Option<LayoutKey>,
    /// The projection the cached layout was shaped from — kept so the cheap
    /// pass can address lines without re-projecting.
    preview: Option<crate::text::diff::DiffPreview>,
    /// The cached layout's wrapped visual rows, block-local logical px. The
    /// selection bands are derived from these, which is why they need no
    /// Parley of their own.
    rows: Vec<crate::text::diff::PreviewRow>,
    /// The cached parley layout itself, kept for the **cursor**: with column
    /// motions back (slice 6) the cursor is a point inside a line, not a row
    /// box, and only the layout knows where a column sits — especially on a
    /// wrapped line, where a late column belongs to a later visual row.
    /// Querying it is a search over the cached lines, not a re-shape, so the
    /// cheap pass stays cheap.
    layout: Option<parley::Layout<peniko::Brush>>,
    /// How many entries at the FRONT of `MsdfBlockGlyphs::glyphs` are the
    /// body. The status strip is appended after them and remade every pass.
    body_glyphs: usize,
    /// Same, for `MsdfBlockGeometry::vertices`: the `+`/`-` bands and the
    /// word washes are the prefix, selection and minimap are appended after
    /// them.
    body_bands: usize,
    /// The minimap's document layer, cached against [`MinimapKey`].
    ///
    /// **Its own tier, coarser than the cursor's.** Building buckets is
    /// O(drawn rows) — a hundred thousand of them on a big diff — and it can
    /// only change when the diff or its folds do, which is far less often
    /// than the cursor moves. So it is built against a key of its own and
    /// *memcpy'd* into the geometry buffer on each cheap pass; only the
    /// viewport band and the cursor line are rebuilt per keystroke. It never
    /// joins [`LayoutKey`]: a minimap redraw must never re-shape a glyph.
    minimap_quads: Vec<crate::text::msdf::geometry::GeometryVertex>,
    /// How many slots `minimap_quads` was built for — the position layer has
    /// to bucket into the same space.
    minimap_slots: usize,
    /// What `minimap_quads` was built for.
    minimap: Option<MinimapKey>,
    /// What the last cheap pass drew.
    drawn: Option<CursorKey>,
}

/// The inputs the **text shaping** depends on. Deliberately free of cursor
/// state: that is what keeps a line move off the Parley path.
///
/// Sizes ride as raw f32 bits rather than truncated integers: the surface is
/// laid out in *logical* px, which a fractional HiDPI scale factor makes
/// fractional, and a resize that stays inside one integer can still move a
/// wrap point. Exact bits cannot be too sensitive here — a size that did not
/// change produces identical bits.
#[derive(PartialEq, Eq, Clone, Debug)]
struct LayoutKey {
    content_hash: u64,
    fold_seq: u64,
    first_row: usize,
    end_row: usize,
    width_bits: u32,
    height_bits: u32,
    /// Font size shapes every glyph and sets the row height the window is
    /// measured in. It is fixed today, which is exactly why leaving it out
    /// would be a trap for whoever makes it adjustable.
    font_bits: u32,
}

/// The inputs the **minimap's document layer** depends on: the whole drawn
/// document and the box it is compressed into. Deliberately coarser than
/// [`CursorKey`] — a cursor move slides the viewport band, it does not
/// re-bucket a hundred thousand rows — and deliberately not part of
/// [`LayoutKey`], because a strip rebuild must never re-shape text.
#[derive(PartialEq, Eq, Clone, Debug)]
struct MinimapKey {
    content_hash: u64,
    fold_seq: u64,
    rows: usize,
    slots: usize,
    /// The strip's box. A resize moves and re-scales every bar.
    strip_bits: [u32; 4],
}

/// The inputs the **cheap pass** depends on — everything that can change
/// without re-shaping a glyph.
#[derive(PartialEq, Eq, Clone, Debug)]
struct CursorKey {
    cursor_row: usize,
    cursor_col: usize,
    selection: Option<(usize, usize)>,
    /// The character-wise extent, when there is one. Rows alone cannot key it:
    /// `vll` never leaves its row and must still redraw.
    char_selection: Option<kaijutsu_diff::CharSpan>,
    stale: bool,
    status: String,
    kind: CursorKind,
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
                MsdfBlockGeometry::default(),
                crate::view::block_render::msdf_surface_bundle(material),
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

    // The strip eats into the text column: the minimap is beside the text, not
    // over it. `width_bits` in `LayoutKey` already keys the surface width, so
    // the narrower column re-wraps correctly on a resize.
    let content_width = (width - 2.0 * PAD - MINIMAP_WIDTH - MINIMAP_GAP).max(0.0);
    let strip = crate::shaders::SelectionRect::new(
        width - PAD - MINIMAP_WIDTH,
        TOP_MARGIN,
        MINIMAP_WIDTH,
        (height - TOP_MARGIN - STATUS_BOTTOM_MARGIN).max(0.0),
    );
    let minimap_slots = ((strip.height / MINIMAP_SLOT_PX).floor() as i64).max(1) as usize;
    let line_height = text_metrics.cell_font_size.max(1.0) * 1.3;
    // How many rows FIT on screen — not to be confused with
    // `DiffCore::visible_rows`, which is how many rows folding leaves to draw.
    let viewport_rows = (((height - TOP_MARGIN - STATUS_BOTTOM_MARGIN) / line_height).floor()
        as i64)
        .max(1) as usize;

    // ── where we are ────────────────────────────────────────────────────────
    // Everything below is in VISIBLE row coordinates — indices into
    // `DiffCore::visible_rows`, which drops a folded hunk's body. The core
    // publishes the cursor and the selection in the same coordinates, and
    // `view_rows` projects the same range, so the whole surface agrees;
    // yank and the canonical model never see a fold at all.
    let (first_row, end_row, cursor_row, cursor_col, selection, char_selection, kind) = match
        &session.content
    {
        DiffViewContent::Ready(core) => {
            let cursor = core.cursor_visible_row();
            let (first, end) = window_for(
                cursor,
                viewport_rows,
                core.visible_rows().len(),
                (window.first_row, window.end_row),
            );
            (
                first,
                end,
                cursor,
                core.cursor_col(),
                core.selection_visible_rows(),
                core.char_selection_visible(),
                // The viewer is never in insert mode, so the beam shape would
                // be a lie. Normal → block; visual → hidden, because the
                // selection bands are the visible thing there.
                mode_kind(core.mode().as_deref()),
            )
        }
        // The error state has nothing to navigate: one fixed "window", no
        // cursor at all.
        DiffViewContent::Failed { .. } => (0, 0, 0, 0, None, None, CursorKind::Hidden),
    };

    let layout_key = LayoutKey {
        content_hash: session.content_hash,
        // Folding changes what the window projects without changing a byte of
        // content, and two folds of equal size between frames would leave even
        // the row count unchanged. The core's own counter is the honest key.
        fold_seq: match &session.content {
            DiffViewContent::Ready(core) => core.fold_seq(),
            DiffViewContent::Failed { .. } => 0,
        },
        first_row,
        end_row,
        width_bits: width.to_bits(),
        height_bits: height.to_bits(),
        font_bits: text_metrics.cell_font_size.to_bits(),
    };
    let cursor_key = CursorKey {
        cursor_row,
        cursor_col,
        selection,
        char_selection,
        stale: session.stale,
        status: status_line(session, cursor_row),
        kind,
    };
    // Theme colors are baked into the cached layout (as ranged brushes) and
    // into the cached band geometry, and the kernel can replace the whole
    // `Theme` resource over RPC at any moment (`apply_theme_from_rpc`). No
    // key can see that — Bevy's own change detection can, so it joins the
    // relayout condition rather than being hashed into one.
    let relayout = window.laid_out.as_ref() != Some(&layout_key) || theme.is_changed();
    if !relayout && window.drawn.as_ref() == Some(&cursor_key) {
        return;
    }

    let fallback = bevy_color_to_brush(theme.diff_context_fg);
    let style = VelloTextStyle {
        brush: fallback.clone(),
        font_size: text_metrics.cell_font_size,
        ..default()
    };
    let text_offset = (PAD as f64, TOP_MARGIN as f64);
    let offset = (PAD, TOP_MARGIN);

    // ── the expensive half: shape the window's text ─────────────────────────
    // Skipped entirely on a cursor-only change, which is what makes `j` free.
    if relayout {
        let preview = match &session.content {
            DiffViewContent::Ready(core) => {
                crate::text::diff::view_rows(core.as_ref(), first_row..end_row)
            }
            DiffViewContent::Failed { reason, source } => {
                crate::text::diff::error_view(source, reason)
            }
        };

        let span_brushes = crate::text::rich::build_diff_span_brushes(&preview, &theme);
        // Spanned, not plain: word-level highlights start *inside* a line, and
        // only parley's own ranged brushes split a shaping run there.
        let layout = font.layout_spanned(
            &preview.plain_text,
            &style,
            VelloTextAlign::Left,
            Some(content_width),
            &span_brushes,
        );
        for line in layout.lines() {
            for item in line.items() {
                if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                    font_data_map.register(gr.run().font());
                }
            }
        }

        let body = collect_msdf_glyphs_styled(&layout, text_offset, atlas);

        // One entry per WRAPPED visual row, addressed by its start byte — a
        // long `+` line must be tinted across every row it occupies. Kept so
        // the cheap pass can place the cursor and the selection bands without
        // Parley: in a line-wise viewer both are row boxes, nothing more.
        let rows: Vec<crate::text::diff::PreviewRow> = layout
            .lines()
            .map(|line| {
                let m = line.metrics();
                crate::text::diff::PreviewRow {
                    text_offset: line.text_range().start,
                    // parley 0.9 split min/max_coord per axis; 0.7's pair was
                    // the line's top/bottom, i.e. the block axis.
                    top: m.block_min_coord,
                    bottom: m.block_max_coord,
                }
            })
            .collect();

        let mut bands = crate::text::diff::build_diff_band_geometry(
            &preview,
            &rows,
            content_width,
            offset,
            &crate::text::rich::diff_band_colors(&theme),
        );
        // Word washes ride the LAYOUT tier beside the line bands — word spans
        // change only when the content does — and are appended *after* them,
        // so a changed word draws as a brighter patch inside its line's band.
        bands.extend(crate::text::diff::build_word_wash_geometry(
            &preview,
            &layout,
            offset,
            &crate::text::rich::diff_word_colors(&theme),
        ));

        window.body_glyphs = body.len();
        window.body_bands = bands.len();
        glyphs.glyphs = body;
        geometry.vertices = bands;

        rtt.built_width = width;
        rtt.built_height = height;
        scene.text = preview.plain_text.clone();
        scene.content_version = scene.content_version.wrapping_add(1);
        scene.last_built_version = scene.content_version;
        scene.scene_version = scene.scene_version.wrapping_add(1);

        window.first_row = first_row;
        window.end_row = end_row;
        window.rows = rows;
        window.preview = Some(preview);
        window.layout = Some(layout);
        window.laid_out = Some(layout_key);
    }

    // ── the cheap half: status strip, selection, minimap, cursor ────────────
    // Everything below rides the cached layout: the body glyphs, `+`/`-` bands
    // and word washes are the untouched prefix of each buffer, and these are
    // appended after them.
    if window.preview.is_none() {
        return;
    }

    // The minimap's document layer is refreshed FIRST, while nothing else
    // holds a borrow of the window — building it is O(drawn rows) and it can
    // only change when the diff or its folds do, so it has a key of its own
    // (`MinimapKey`), coarser than the cursor's and outside `LayoutKey`
    // entirely. Drawing it below is then a memcpy.
    let minimap_colors = crate::text::rich::diff_minimap_colors(&theme);
    if let DiffViewContent::Ready(core) = &session.content {
        let minimap_key = MinimapKey {
            content_hash: session.content_hash,
            fold_seq: core.fold_seq(),
            rows: core.visible_rows().len(),
            slots: minimap_slots,
            strip_bits: [
                strip.x.to_bits(),
                strip.y.to_bits(),
                strip.width.to_bits(),
                strip.height.to_bits(),
            ],
        };
        if window.minimap.as_ref() != Some(&minimap_key) || theme.is_changed() {
            window.minimap_quads = crate::text::diff::build_minimap_geometry(
                &core.minimap(minimap_slots),
                strip,
                &minimap_colors,
            );
            window.minimap_slots = minimap_slots;
            window.minimap = Some(minimap_key);
        }
    }

    let preview = window.preview.as_ref().expect("checked just above");
    glyphs.glyphs.truncate(window.body_glyphs);
    geometry.vertices.truncate(window.body_bands);

    // The status strip: one short line, so re-laying it out per keystroke is
    // free next to the body.
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
        &cursor_key.status,
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
    glyphs.glyphs.extend(collect_msdf_glyphs(
        &strip_layout,
        &[],
        &strip_style.brush,
        strip_offset,
        atlas,
    ));

    // Selection rides ON TOP of the +/- bands: a selected insertion should
    // read as both. Row indices are window-relative.
    //
    // Two shapes, two paths. Line-wise (`V`) is full-width bands and needs no
    // Parley at all — a whole row is a whole row. Character-wise (`v`) is the
    // ragged shape the multi-rect work exists for: Parley measures where the
    // columns actually are, `coalesce_selection_rects` folds the interior rows
    // into one full-width band (`shaders::selection`), and the result is the
    // same three-rect shape the editor panel will push into the material.
    match (char_selection, selection) {
        (Some(span), _) => {
            let local = kaijutsu_diff::CharSpan {
                // A selection reaching above the window starts at the window's
                // first row, from its first column — the columns of a row that
                // was never laid out mean nothing here.
                start_row: span.start_row.saturating_sub(window.first_row),
                start_col: if span.start_row >= window.first_row {
                    span.start_col
                } else {
                    0
                },
                end_row: span.end_row.saturating_sub(window.first_row),
                // A column only means something on the row it was measured
                // on. If the selection ends below the laid-out window, the
                // end row is not this one, so the column travelling with it
                // would be a measurement of a different line —
                // `char_selection_bytes` clamps that case to "the last row,
                // whole", and this says so rather than leaving a live-looking
                // value for a future refactor to start reading.
                end_col: if span.end_row.saturating_sub(window.first_row) < preview.lines.len() {
                    span.end_col
                } else {
                    usize::MAX
                },
            };
            if span.end_row >= window.first_row
                && let Some(bytes) = crate::text::diff::char_selection_bytes(preview, local)
                && let Some(layout) = window.layout.as_ref()
            {
                let rects = crate::text::diff::layout_rects(layout, bytes);
                let coalesced = crate::shaders::selection::coalesce_selection_rects(
                    &rects,
                    0.0,
                    content_width,
                );
                geometry
                    .vertices
                    .extend(crate::text::diff::selection_rect_geometry(
                        &coalesced,
                        offset,
                        rgba8(theme.selection_bg),
                    ));
            }
        }
        (None, Some((a, b))) if b >= window.first_row => {
            let local = (
                a.saturating_sub(window.first_row),
                b.saturating_sub(window.first_row),
            );
            geometry
                .vertices
                .extend(crate::text::diff::build_selection_band_geometry(
                    preview,
                    &window.rows,
                    local,
                    content_width,
                    offset,
                    rgba8(theme.selection_bg),
                ));
        }
        _ => {}
    }

    // The minimap: the cached document layer, then the two marks that follow
    // the reader — the band of rows on screen and the cursor's own line.
    if let DiffViewContent::Ready(core) = &session.content {
        geometry.vertices.extend_from_slice(&window.minimap_quads);
        geometry
            .vertices
            .extend(crate::text::diff::build_minimap_position_geometry(
                core.visible_rows().len(),
                window.minimap_slots,
                // What is actually on SCREEN, not what was laid out: the
                // window lays out slack rows past the bottom edge that the
                // reader cannot see, and a band claiming them would overstate
                // where they are.
                first_row..end_row.min(first_row + viewport_rows),
                cursor_row,
                strip,
                &minimap_colors,
            ));
    }

    // Geometry and glyphs are published together under the single shared
    // version gate (`MsdfBlockGeometry`'s contract: fill both, bump once).
    glyphs.version = glyphs.version.wrapping_add(1);

    // The cursor is a point inside a line again (slice 6): column motions are
    // back, so it is drawn where it actually is. The cached layout answers
    // where that is — a lookup over lines already shaped, not a re-shape —
    // and it is the only thing that gets a *wrapped* line right, where a late
    // column belongs to a later visual row.
    let local = cursor_row.saturating_sub(window.first_row);
    let byte = crate::text::diff::cursor_byte(preview, local, cursor_col);
    match (byte, window.layout.as_ref()) {
        (Some(byte), Some(layout)) => {
            let geom = parley::editing::Cursor::from_byte_index(
                layout,
                byte,
                parley::layout::Affinity::Downstream,
            )
            .geometry(layout, 2.0);
            cursor_geom.x = text_offset.0 + geom.x0;
            cursor_geom.y = text_offset.1 + geom.y0;
            cursor_geom.height = geom.y1 - geom.y0;
            cursor_geom.last_cursor_offset = byte;
        }
        _ => {
            cursor_geom.height = 0.0;
            cursor_geom.last_cursor_offset = 0;
        }
    }
    cursor_geom.kind = kind;

    window.drawn = Some(cursor_key);
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
            // An empty diff has no line 1 to be on; `1/1` would be a claim
            // about content that isn't there. The count is of *drawn* rows —
            // it has to agree with the position beside it, and that position
            // is where the cursor is on screen.
            let total = core.visible_rows().len();
            if total > 0 {
                s.push_str(&format!("  {}/{total}", cursor_row + 1));
            }
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
        mat.selection_rects = sp;
        mat.selection_color = sc;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The layout key for a given cursor position, composing the two rules
    /// that decide whether the frame re-shapes text.
    fn layout_key_at(
        cursor: usize,
        visible: usize,
        total: usize,
        current: (usize, usize),
    ) -> LayoutKey {
        let (first_row, end_row) = window_for(cursor, visible, total, current);
        LayoutKey {
            content_hash: 7,
            fold_seq: 0,
            first_row,
            end_row,
            width_bits: 1920.0f32.to_bits(),
            height_bits: 1080.0f32.to_bits(),
            font_bits: 20.0f32.to_bits(),
        }
    }

    /// **The invariant the two-key split exists for**: a `j`/`k` inside the
    /// laid-out band must not re-shape a glyph. The layout key is what gates
    /// Parley, so an unchanged key across a cursor move *is* "no relayout".
    #[test]
    fn a_cursor_move_inside_the_window_does_not_change_the_layout_key() {
        let (visible, total) = (40, 100_000);
        let band = window_for(0, visible, total, (0, 0));
        let base = layout_key_at(0, visible, total, band);

        // Every row of the visible band. (The slack rows past it are laid
        // out but are NOT roaming room — the cursor must stay on screen.)
        for cursor in [1, 2, 17, visible - 1] {
            assert_eq!(
                layout_key_at(cursor, visible, total, band),
                base,
                "cursor {cursor} re-shaped the window",
            );
        }
    }

    /// ...and leaving it *must* re-shape, or the reader would scroll into
    /// text that was never laid out.
    #[test]
    fn leaving_the_window_does_change_the_layout_key() {
        let (visible, total) = (40, 100_000);
        let band = window_for(0, visible, total, (0, 0));
        let base = layout_key_at(0, visible, total, band);
        assert_ne!(
            layout_key_at(visible, visible, total, band),
            base,
            "one row past the bottom edge must slide the band",
        );
        assert_ne!(layout_key_at(5_000, visible, total, band), base);
    }

    /// A resize re-shapes even with the cursor still put — wrapping changes.
    #[test]
    fn a_resize_changes_the_layout_key() {
        let band = (0, 64);
        let a = layout_key_at(0, 40, 100_000, band);
        let mut b = a.clone();
        b.width_bits = 1280.0f32.to_bits();
        assert_ne!(a, b);

        // ...including a resize too small to change a truncated integer,
        // which can still move a wrap point.
        let mut c = a.clone();
        c.width_bits = 1920.4f32.to_bits();
        assert_ne!(a, c, "a fractional logical resize must re-shape");

        // ...and a font-size change with the window untouched.
        let mut d = a.clone();
        d.font_bits = 22.0f32.to_bits();
        assert_ne!(a, d);
    }

    /// The cheap pass still has to run on a cursor move — the status counter,
    /// the cursor box, and the selection bands all follow it.
    #[test]
    fn a_cursor_move_does_change_the_cheap_pass_key() {
        let key = |cursor_row| CursorKey {
            cursor_row,
            cursor_col: 0,
            selection: None,
            char_selection: None,
            stale: false,
            status: format!("diff  {}/100", cursor_row + 1),
            kind: CursorKind::Block,
        };
        assert_ne!(key(0), key(1));
        assert_eq!(key(3), key(3));
    }

    /// A character-wise selection that never leaves its row still has to
    /// redraw — `vll` changes nothing the row range can see.
    #[test]
    fn widening_a_character_selection_changes_the_cheap_pass_key() {
        let key = |end_col| CursorKey {
            cursor_row: 4,
            cursor_col: end_col,
            selection: Some((4, 4)),
            char_selection: Some(kaijutsu_diff::CharSpan {
                start_row: 4,
                start_col: 1,
                end_row: 4,
                end_col,
            }),
            stale: false,
            status: "diff  5/100".into(),
            kind: CursorKind::Hidden,
        };
        assert_ne!(key(3), key(6));
        assert_eq!(key(3), key(3));
    }

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
