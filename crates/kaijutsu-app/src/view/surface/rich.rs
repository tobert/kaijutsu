//! Rich content on the surface: geometry, rasters, and the one-chunk shaping
//! that produces them.
//!
//! Four of the seven [`RichContentKind`]s draw something parley cannot — an
//! engraved staff, a diff's background bands, a sparkline plot, an image
//! placeholder — and one (SVG) draws a CPU raster. This module is where those
//! are built, from **exactly the builders the legacy path calls**
//! (`view/block_render.rs`'s arms): `collect_music_geometry` /
//! `collect_music_glyphs` / `collect_music_text_glyphs` for ABC,
//! `build_diff_band_geometry` + `build_word_wash_geometry` for diffs,
//! `build_sparkline_vertices` for sparklines, `rasterize_svg` for SVG. Sharing
//! the builders is the whole point: two renderers that agree about what a
//! diff *is* but disagree about where its bands go would be worse than one
//! renderer.
//!
//! # Three rules the drawn kinds live by
//!
//! - **One chunk, never chunked.** A diff's word washes are addressed by byte
//!   offsets into one parley layout, and an engraved score has no lines at
//!   all; chunking either would mean re-deriving those offsets per chunk for
//!   no benefit. All of these are bounded by construction (the diff preview's
//!   line budget, the engraver's own output), so the reason chunking exists —
//!   a streamed block that grows without limit — does not apply.
//! - **Main thread only.** ABC glyph collection mutates [`MsdfAtlas`], and the
//!   colors these bake come from [`Theme`] rather than from the block's own
//!   color. Neither travels to `AsyncComputeTaskPool` today, and the work is
//!   small and rare enough that sending it there would be machinery without a
//!   payoff. `shape_visible_blocks` keeps them off the backlog.
//! - **Theme colors are a shaping input, for these kinds only.** A diff band
//!   is `theme.diff_insert_bg`, not the block's text color, so a theme swap
//!   must re-build the geometry — which is why [`super::shape_cache::ShapeKey`]
//!   carries `rich_theme_epoch`, and why it is zero for everything else (a
//!   theme swap must never re-shape the whole conversation; that is the
//!   full-document rebake this rewrite deletes).
//!
//! # Accepted limitation: cross-lane snap (≤1 physical px)
//!
//! Glyphs snap each baseline to a physical row; geometry snaps one anchor
//! per run and keeps its vertices rigid (a 1px staff line must not vanish or
//! double). The two roundings can disagree by up to one physical pixel, so
//! an ABC notehead can sit ≤1px off its staff line, jittering with scroll.
//! Inherent to the two-lane design — per-vertex snapping would deform the
//! geometry, per-run glyph snapping would unspace the text. Reviewed and
//! accepted 2026-08-18; revisit only if it reads as wrong at 1x.

use std::collections::HashMap;
use std::sync::Arc;

use bevy::prelude::*;
use kaijutsu_types::BlockId;

use crate::cell::{ConversationScrollState, EditorEntities};
use crate::text::components::{bevy_color_to_brush, color_to_rgba8};
use crate::text::msdf::geometry::{GeometryVertex, rect_quad};
use crate::text::msdf::layout_bridge::{
    collect_msdf_glyphs_deferred, collect_msdf_glyphs_styled_deferred,
};
use crate::text::msdf::music_bridge::{
    collect_music_geometry, collect_music_glyphs, collect_music_text_glyphs,
};
use crate::text::msdf::{FontDataMap, MsdfAtlas};
use crate::text::rich::{RichContentKind, SVG_MAX_HEIGHT, SpanBrush};
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};
use crate::text::sparkline::{SparklineColors, build_sparkline_vertices};
use crate::text::svg_raster::{SVG_RASTER_MAX_DIM, fit_svg_to_box, rasterize_svg};
use crate::text::TextMetrics;
use crate::ui::theme::Theme;
use crate::view::block_render::{GpuTextureLimits, IMAGE_PLACEHOLDER_COLOR, create_svg_raster_image};
use crate::view::geometry::{ConversationGeometry, RowKey};
use crate::view::ui_rtt::ui_rtt_texture_dims;

use super::content::{BlockContentCache, RichKindInfo};
use super::shape_cache::{SHAPE_SLACK_SCREENS, ShapeOutput, ShapedChunk, collect_fonts};

/// Height of the `Image` placeholder rectangle, verbatim from
/// `build_block_scenes`' Image arm.
pub const IMAGE_PLACEHOLDER_HEIGHT: f32 = 120.0;

/// Padding the sparkline plot keeps inside its box (legacy passes `4.0`).
pub const SPARKLINE_PADDING: f32 = 4.0;

/// Everything the drawn kinds need that isn't per-block.
///
/// Bundled because three of the four builders want the atlas *and* the font
/// map *and* the theme, and a five-argument free function would put the
/// borrow checker in charge of the signature.
pub struct RichShaper<'a> {
    pub font: &'a VelloFont,
    pub theme: &'a Theme,
    pub atlas: &'a mut MsdfAtlas,
    pub font_data: &'a mut FontDataMap,
}

impl RichShaper<'_> {
    /// Shape one rich block into a single chunk carrying both its glyphs and
    /// its geometry.
    ///
    /// `text` / `spans` are what `super::content::detect` decided this kind
    /// shapes (empty for the drawn-only kinds); `wrap_width` is the block's
    /// content width — `chrome::BlockLayout::wrap_width`, which is legacy's
    /// `width - pad_left - pad_right`. Geometry and glyphs both come out in
    /// **chunk-local** coordinates (origin at the block's text origin), so
    /// assembly adds the document y and the indent x exactly as it does for
    /// plain text; legacy bakes `(pad_left, pad_top)` in at build time
    /// instead, which is the same placement reached from the other side.
    pub fn shape(
        &mut self,
        kind: &RichContentKind,
        text: &str,
        spans: &[SpanBrush],
        style: &VelloTextStyle,
        wrap_width: f32,
    ) -> ShapeOutput {
        match kind {
            RichContentKind::Abc { tune, .. } => self.shape_abc(tune, wrap_width),
            RichContentKind::Diff(preview) => self.shape_diff(preview, text, spans, style, wrap_width),
            RichContentKind::Sparkline(data) => {
                let height = self.theme.sparkline_height;
                let colors = SparklineColors {
                    line: self.theme.sparkline_line_color,
                    fill: self.theme.sparkline_fill_color,
                };
                let geometry = if wrap_width > 0.0 && height > 0.0 {
                    build_sparkline_vertices(
                        data,
                        wrap_width,
                        height,
                        SPARKLINE_PADDING,
                        (0.0, 0.0),
                        color_to_rgba8(colors.line),
                        colors.fill.map(color_to_rgba8),
                    )
                } else {
                    Vec::new()
                };
                finish(Vec::new(), Vec::new(), geometry, height, text.len())
            }
            RichContentKind::Svg { width, height, .. } => {
                // The raster itself is a separate pass ([`sync_svg_rasters`])
                // because it needs `Assets<Image>`; all that is owed here is
                // the height the row reserves, which is pure and must agree
                // with the size the raster is drawn at.
                let draw_h = svg_draw_size(*width, *height, wrap_width)
                    .map(|(_, h)| h)
                    .unwrap_or(0.0);
                finish(Vec::new(), Vec::new(), Vec::new(), draw_h, text.len())
            }
            RichContentKind::Image { .. } => self.shape_image(text, style, wrap_width),
            // Markdown and Output are rich only in their coloring — they never
            // reach here (`RichKindInfo::is_drawn` is false for both), and if
            // a future kind forgets to say which half it is in, shaping its
            // text as plain is the honest answer rather than drawing nothing.
            RichContentKind::Markdown { .. } | RichContentKind::Output { .. } => {
                let layout = self.font.layout(
                    text,
                    style,
                    VelloTextAlign::Left,
                    (wrap_width > 0.0).then_some(wrap_width),
                );
                let mut fonts = Vec::new();
                collect_fonts(&layout, &mut fonts);
                let (glyphs, keys) =
                    collect_msdf_glyphs_deferred(&layout, spans, &style.brush, (0.0, 0.0));
                let mut out = finish(glyphs, keys, Vec::new(), layout.height(), text.len());
                out.fonts = fonts;
                out
            }
        }
    }

    /// ABC: engrave the tune, scale it to the column, and emit staff geometry
    /// + notation glyphs + text glyphs. Ported from `build_block_scenes`' Abc
    /// arm, including its scale-to-fit (which *does* fill the width — an
    /// engraved score has an aspect ratio, not an intrinsic pixel size).
    fn shape_abc(&mut self, tune: &kaijutsu_abc::Tune, wrap_width: f32) -> ShapeOutput {
        let opts = kaijutsu_abc::engrave::EngravingOptions::default();
        let elements = kaijutsu_abc::engrave::layout::engrave(tune, &opts);
        let bounds = crate::text::abc::compute_engraving_bounds(&elements, opts.margin);
        if bounds.width <= 0.0 || bounds.height <= 0.0 || wrap_width <= 0.0 {
            // `compute_engraving_bounds` guards the empty case, so this is a
            // degenerate column (a pane mid-layout), not bad music. Reserve
            // nothing and re-shape when a real width arrives.
            return finish(Vec::new(), Vec::new(), Vec::new(), 0.0, 0);
        }

        let scale = (wrap_width as f64 / bounds.width).min(SVG_MAX_HEIGHT as f64 / bounds.height);
        let height = (bounds.height * scale) as f32;
        let origin = (bounds.origin_x, bounds.origin_y);
        let brush = bevy_color_to_brush(self.theme.block_assistant);

        let geometry = collect_music_geometry(&elements, (0.0, 0.0), origin, scale, &brush);
        // These two queue their own glyph keys into the atlas — the deferred
        // collector contract is a parley thing, and music glyphs never go
        // through parley (positioned from the engraving IR directly, the
        // locked decision in `music_bridge`).
        let mut glyphs = collect_music_glyphs(
            &elements,
            (0.0, 0.0),
            origin,
            scale,
            &brush,
            self.atlas,
        );
        glyphs.extend(collect_music_text_glyphs(
            &elements,
            (0.0, 0.0),
            origin,
            scale,
            &brush,
            Some(self.font),
            self.atlas,
            self.font_data,
        ));

        finish(glyphs, Vec::new(), geometry, height, 0)
    }

    /// Diff: one spanned layout, its glyphs, its per-line bands, and the
    /// word washes on top of them — the same order and the same builders as
    /// `build_block_scenes`' Diff arm.
    fn shape_diff(
        &mut self,
        preview: &crate::text::diff::DiffPreview,
        text: &str,
        spans: &[SpanBrush],
        style: &VelloTextStyle,
        wrap_width: f32,
    ) -> ShapeOutput {
        let max_advance = (wrap_width > 0.0).then_some(wrap_width);
        // Context is the quietest diff class, so it is the right color for any
        // byte a span somehow misses — the block's own role color (amber, or
        // error red) would be a loud thing to fall back to inside a diff.
        // Legacy makes the same substitution for the same reason.
        let style = VelloTextStyle {
            brush: bevy_color_to_brush(self.theme.diff_context_fg),
            ..style.clone()
        };
        // Spanned, not plain: word-level highlights start *inside* a line, and
        // only parley's own ranged brushes split a shaping run there.
        let layout =
            self.font
                .layout_spanned(text, &style, VelloTextAlign::Left, max_advance, spans);
        let height = layout.height();

        let rows = crate::text::diff::preview_rows(&layout);
        let mut geometry = crate::text::diff::build_diff_band_geometry(
            preview,
            &rows,
            wrap_width,
            (0.0, 0.0),
            &crate::text::rich::diff_band_colors(self.theme),
        );
        geometry.extend(crate::text::diff::build_word_wash_geometry(
            preview,
            &layout,
            (0.0, 0.0),
            &crate::text::rich::diff_word_colors(self.theme),
        ));

        let mut fonts = Vec::new();
        collect_fonts(&layout, &mut fonts);
        let (glyphs, keys) = collect_msdf_glyphs_styled_deferred(&layout, (0.0, 0.0));
        let mut out = finish(glyphs, keys, geometry, height, text.len());
        out.fonts = fonts;
        out
    }

    /// Image placeholder: a dark rectangle with its `[image: …]` caption
    /// centred on it. The rectangle is geometry rather than a chrome quad
    /// because it is *content* — it scrolls, wraps and re-shapes with the
    /// block, and chrome instances are keyed to borders.
    fn shape_image(
        &mut self,
        label: &str,
        style: &VelloTextStyle,
        wrap_width: f32,
    ) -> ShapeOutput {
        let height = IMAGE_PLACEHOLDER_HEIGHT;
        let geometry = rect_quad(
            0.0,
            0.0,
            wrap_width.max(0.0) as f64,
            height as f64,
            color_to_rgba8(IMAGE_PLACEHOLDER_COLOR),
        )
        .to_vec();

        let layout = self
            .font
            .layout(label, style, VelloTextAlign::Left, (wrap_width > 0.0).then_some(wrap_width));
        // Vertically centred in the placeholder, exactly as the legacy arm
        // centres it (`pad_top + h/2 - label_height/2`, minus the padding the
        // surface adds at assembly instead of at build time).
        let label_y = ((height - layout.height()) * 0.5).max(0.0);
        let mut fonts = Vec::new();
        collect_fonts(&layout, &mut fonts);
        let (glyphs, keys) =
            collect_msdf_glyphs_deferred(&layout, &[], &style.brush, (0.0, label_y as f64));

        let mut out = finish(glyphs, keys, geometry, height, label.len());
        out.fonts = fonts;
        out
    }
}

/// Package one rich block's drawing as the single chunk it always is.
fn finish(
    glyphs: Vec<crate::text::msdf::PositionedGlyph>,
    keys: Vec<crate::text::msdf::glyph::GlyphKey>,
    geometry: Vec<GeometryVertex>,
    height: f32,
    text_len: usize,
) -> ShapeOutput {
    ShapeOutput {
        chunks: vec![ShapedChunk {
            byte_range: 0..text_len,
            glyphs: Arc::new(glyphs),
            geometry: Arc::new(geometry),
            height,
            // Never frozen: the incremental-tail path is for streamed text,
            // and a rich block re-derives its drawing whole or not at all.
            frozen: false,
        }],
        text_height: height,
        glyph_keys: keys,
        fonts: Vec::new(),
    }
}

// ============================================================================
// SVG RASTERS
// ============================================================================

/// The logical `(width, height)` an SVG draws at inside a column of
/// `wrap_width`, or `None` for a degenerate SVG or column.
///
/// Shrink-only ([`fit_svg_to_box`]): a 200×200 cat is a 200×200 cat. Shared by
/// the shaper (which owes the row a height) and the raster pass (which owes
/// the GPU a pixel count), so the two cannot disagree about how big the
/// picture is.
pub fn svg_draw_size(svg_w: f32, svg_h: f32, wrap_width: f32) -> Option<(f32, f32)> {
    fit_svg_to_box(svg_w, svg_h, wrap_width, SVG_MAX_HEIGHT).map(|(_, w, h)| (w, h))
}

/// One block's rasterized SVG.
#[derive(Debug, Clone)]
pub struct SvgRaster {
    /// `FormattedBlock::version` this was rasterized from.
    pub content_version: u64,
    /// Physical pixel size of the bitmap — the staleness input a logical
    /// width and a content version between them cannot express (drag the
    /// window to a 2x monitor and neither moves).
    pub physical: (u32, u32),
    /// Logical size the quad draws at.
    pub draw: (f32, f32),
    pub image: Handle<Image>,
}

/// Rasterized SVGs, keyed by block.
///
/// **Lifetime.** An entry is replaced when its `(content_version, physical
/// size)` moves — the old `Handle<Image>` drops with it, and the asset with
/// the handle — and dropped entirely when the block leaves
/// [`BlockContentCache`] (context switch, truncation, exclusion). It is not
/// bounded by a screen band: a raster costs one texture and there are never
/// many, while re-rastering on every scroll past would be visible work. That
/// makes the document, not the viewport, the eviction unit — deliberately
/// different from `ShapedBlockCache`, whose glyphs dominate memory.
#[derive(Resource, Debug, Default)]
pub struct SvgRasterCache {
    entries: HashMap<BlockId, SvgRaster>,
}

// No generation counter here, deliberately: quads are assembled and uploaded
// every frame (`extract::extract_conversation_surfaces`), so a raster
// landing, being replaced, or dropping reaches the GPU on its own. A
// `WindowKey` coupling existed briefly and only forced needless glyph/
// geometry re-uploads — deleted per review, 2026-08-18.

impl SvgRasterCache {
    pub fn get(&self, id: &BlockId) -> Option<&SvgRaster> {
        self.entries.get(id)
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    #[allow(dead_code)] // Model accessor for tests and diagnostics.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether a raster for `(version, physical)` is already held — the
    /// staleness rule, split out so it can be tested without a GPU or an
    /// asset server.
    pub fn is_current(&self, id: &BlockId, content_version: u64, physical: (u32, u32)) -> bool {
        self.entries.get(id).is_some_and(|entry| {
            entry.content_version == content_version && entry.physical == physical
        })
    }

    pub fn insert(&mut self, id: BlockId, raster: SvgRaster) {
        self.entries.insert(id, raster);
    }

    fn retain(&mut self, keep: impl Fn(&BlockId) -> bool) {
        self.entries.retain(|id, _| keep(id));
    }
}

/// The physical pixel size an SVG rasterizes at, and the scale that maps SVG
/// user units straight into it.
///
/// Capped by [`SVG_RASTER_MAX_DIM`] as well as the GPU's texture limit: the
/// GPU limit bounds a *texture*, while this bitmap is first a plain CPU
/// `Vec<u8>`, and some GPUs report limits that would make a single block's
/// raster a several-hundred-megabyte allocation.
pub fn svg_raster_target(
    svg_w: f32,
    svg_h: f32,
    wrap_width: f32,
    scale_factor: f32,
    max_texture_dim: u32,
) -> Option<((u32, u32), f64, (f32, f32))> {
    let (fit_scale, draw_w, draw_h) = fit_svg_to_box(svg_w, svg_h, wrap_width, SVG_MAX_HEIGHT)?;
    let max_dim = max_texture_dim.min(SVG_RASTER_MAX_DIM);
    let target = ui_rtt_texture_dims(draw_w, draw_h, scale_factor, max_dim);
    // `fit_scale` maps SVG user units to LOGICAL px; chaining the DPI scale
    // maps them straight to PHYSICAL px in one pass, so the bitmap is sized
    // and scaled for its actual on-screen resolution instead of being an
    // upscaled logical-res intermediate.
    Some((target, fit_scale * scale_factor as f64, (draw_w, draw_h)))
}

/// Keep [`SvgRasterCache`] in step with the SVG blocks in the shape band.
///
/// Its own system rather than part of the shape pass because rasterizing
/// needs `Assets<Image>`, which the shaper deliberately does not touch. It
/// runs in the same set, after shaping, so the wrap widths both read are the
/// frame's.
#[allow(clippy::too_many_arguments)]
pub fn sync_svg_rasters(
    entities: Res<EditorEntities>,
    geometries: Query<&ConversationGeometry>,
    computed_nodes: Query<&ComputedNode>,
    scroll_state: Res<ConversationScrollState>,
    text_metrics: Res<TextMetrics>,
    theme: Res<Theme>,
    gpu_limits: Res<GpuTextureLimits>,
    content: Res<BlockContentCache>,
    chrome: Res<super::chrome::BlockChromeCache>,
    mut images: ResMut<Assets<Image>>,
    mut cache: ResMut<SvgRasterCache>,
) {
    // Blocks that left the document keep neither text nor pixels — and a
    // block that STOPPED BEING AN SVG loses its raster too. Retaining on
    // document liveness alone left the old bitmap behind when a block's
    // content was edited to another kind: the band walk below skips
    // non-SVG blocks without dropping their entry, and `assemble_quads`
    // draws any raster it finds, so the stale image sat behind the new
    // text indefinitely (found by review, 2026-08-18). Cheap to ask first:
    // the common case is an empty cache and an early return.
    if !cache.is_empty() {
        cache.retain(|id| {
            content
                .get(id)
                .is_some_and(|f| f.rich == Some(RichKindInfo::Svg))
        });
    }

    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(geom) = geometries.get(main_ent) else {
        return;
    };
    let container_width = entities
        .conversation_container
        .and_then(|e| computed_nodes.get(e).ok())
        .map(|c| crate::view::ui_rtt::logical_content_size(c).x)
        .filter(|w| *w > 1.0);
    let Some(container_width) = container_width else {
        return;
    };

    let vh = if scroll_state.visible_height > 0.0 {
        scroll_state.visible_height
    } else {
        600.0
    };
    let band = geom.visible_rows(scroll_state.offset, vh, SHAPE_SLACK_SCREENS * vh);

    for row in &geom.rows()[band] {
        let RowKey::Block(id) = row.key else {
            continue;
        };
        let Some(formatted) = content.get(&id) else {
            continue;
        };
        if formatted.rich != Some(RichKindInfo::Svg) {
            continue;
        }
        let Some(RichContentKind::Svg {
            tree,
            width,
            height,
            ..
        }) = formatted.rich_content.as_ref().map(|p| p.kind())
        else {
            continue;
        };

        let layout = chrome.layout(&id, container_width, row.indent_level, theme.indent_width);
        let Some((target, content_scale, draw)) = svg_raster_target(
            *width,
            *height,
            layout.wrap_width,
            text_metrics.scale_factor,
            gpu_limits.max_texture_dim,
        ) else {
            // A degenerate column — the block's layout hasn't settled. Not a
            // content problem, and the next frame's real width re-runs this.
            continue;
        };

        if cache.is_current(&id, formatted.version, target) {
            continue;
        }

        let Some(rgba) = rasterize_svg(tree, target.0, target.1, content_scale) else {
            // `ui_rtt_texture_dims` clamps both dimensions to >= 1, so
            // tiny-skia refusing the pixmap should be unreachable. Recording
            // the attempt anyway keeps a persistent failure at a stable size
            // from re-rasterizing every frame.
            error!(
                "SVG rasterize failed at {}x{} physical px (should be unreachable)",
                target.0, target.1
            );
            cache.insert(
                id,
                SvgRaster {
                    content_version: formatted.version,
                    physical: target,
                    draw,
                    image: Handle::default(),
                },
            );
            continue;
        };

        let image = create_svg_raster_image(&mut images, target.0, target.1, rgba);
        cache.insert(
            id,
            SvgRaster {
                content_version: formatted.version,
                physical: target,
                draw,
                image,
            },
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{ContextId, PrincipalId};

    fn mono() -> VelloFont {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/fonts/NotoMono-Regular.ttf"
        ))
        .expect("shipped test font must be present");
        crate::text::shaping::load_into_font_context(bytes)
    }

    /// One rich shaping, with the smallest real atlas the pass can be given.
    fn shape_one(kind: &RichContentKind, text: &str, wrap_width: f32) -> ShapeOutput {
        let font = mono();
        let theme = Theme::default();
        let mut images = Assets::<Image>::default();
        let mut atlas = MsdfAtlas::new(&mut images, 128, 128);
        let mut font_data = FontDataMap::default();
        let style = super::super::shape_cache::block_text_style(
            &TextMetrics::default(),
            bevy_color_to_brush(Color::WHITE),
        );
        RichShaper {
            font: &font,
            theme: &theme,
            atlas: &mut atlas,
            font_data: &mut font_data,
        }
        .shape(kind, text, &[], &style, wrap_width)
    }

    /// A sparkline is geometry and nothing else, at exactly the height the
    /// theme reserves — the height the row was measured at.
    #[test]
    fn a_sparkline_shapes_into_one_geometry_chunk_at_the_theme_height() {
        let data = crate::text::sparkline::try_parse_sparkline("```sparkline\n1 3 7 2\n```")
            .expect("test premise: the fence parses");
        let out = shape_one(&RichContentKind::Sparkline(data), "", 200.0);

        assert_eq!(out.chunks.len(), 1, "rich content is never chunked");
        assert_eq!(out.text_height, Theme::default().sparkline_height);
        assert!(!out.chunks[0].geometry.is_empty(), "the plot is triangles");
        assert!(out.chunks[0].glyphs.is_empty(), "a sparkline has no text");
        assert!(!out.chunks[0].frozen, "nothing here is a streamed prefix");
    }

    /// A degenerate column plots nothing but still reserves its height, so
    /// the row doesn't jump when a real width arrives.
    #[test]
    fn a_sparkline_in_a_zero_width_column_reserves_height_without_geometry() {
        let data = crate::text::sparkline::try_parse_sparkline("```sparkline\n1 2\n```").unwrap();
        let out = shape_one(&RichContentKind::Sparkline(data), "", 0.0);
        assert_eq!(out.text_height, Theme::default().sparkline_height);
        assert!(out.chunks[0].geometry.is_empty());
    }

    /// The image placeholder: one filled rectangle (two triangles) plus its
    /// caption's glyphs, centred vertically inside the rect.
    #[test]
    fn an_image_placeholder_is_a_rect_plus_a_centred_label() {
        let label = super::super::content::image_placeholder_label("0123456789abcdef");
        assert_eq!(label, "[image: 01234567]");

        let out = shape_one(
            &RichContentKind::Image { hash: "0123456789abcdef".into() },
            &label,
            300.0,
        );
        assert_eq!(out.text_height, IMAGE_PLACEHOLDER_HEIGHT);
        assert_eq!(out.chunks[0].geometry.len(), 6, "one quad, two triangles");
        assert!(!out.chunks[0].glyphs.is_empty(), "the caption must be drawn");
        let top = out.chunks[0].glyphs.iter().map(|g| g.y).fold(f32::MAX, f32::min);
        assert!(
            top > 0.0 && top < IMAGE_PLACEHOLDER_HEIGHT,
            "the caption sits inside the placeholder, not on its top edge: {top}",
        );
    }

    /// An SVG shapes no glyphs and no geometry — its picture is a textured
    /// quad — but it must still reserve the height it will draw at, or the
    /// row collapses until the raster lands.
    #[test]
    fn an_svg_reserves_its_draw_height_and_nothing_else() {
        let out = shape_one(
            &RichContentKind::Svg {
                tree: svg_tree(),
                width: 100.0,
                height: 50.0,
                source: Arc::new(String::new()),
            },
            "",
            400.0,
        );
        assert_eq!(out.text_height, 50.0, "shrink-only: it draws at its intrinsic size");
        assert!(out.chunks[0].glyphs.is_empty());
        assert!(out.chunks[0].geometry.is_empty());
    }

    /// The whole ABC port, end to end: engrave → staff geometry + notation
    /// glyphs, scaled to fill the column. The height is the engraving's own
    /// aspect ratio applied to the width, which is where the row's reserved
    /// space comes from — the one number a wrong port would get silently
    /// wrong.
    #[test]
    fn an_abc_tune_shapes_into_staff_geometry_and_notation_glyphs() {
        let tunes = kaijutsu_abc::parse("X:1\nT:t\nK:C\nCDEF|GABc|\n").value;
        let tune = Arc::new(tunes.into_iter().next().expect("test premise: one tune"));

        let wide = shape_one(
            &RichContentKind::Abc {
                source: Arc::new(String::new()),
                tune: tune.clone(),
            },
            "",
            600.0,
        );
        assert!(!wide.chunks[0].geometry.is_empty(), "staff lines are geometry");
        assert!(!wide.chunks[0].glyphs.is_empty(), "noteheads are MSDF glyphs");
        assert!(wide.text_height > 0.0);

        // Half the column, same tune: scale-to-fit means half the height.
        let narrow = shape_one(
            &RichContentKind::Abc {
                source: Arc::new(String::new()),
                tune,
            },
            "",
            300.0,
        );
        assert!(
            (narrow.text_height * 2.0 - wide.text_height).abs() < 0.5,
            "an engraving fills the width and keeps its aspect: {} vs {}",
            narrow.text_height,
            wide.text_height,
        );
    }

    fn svg_tree() -> Arc<usvg::Tree> {
        let source = r#"<svg xmlns="http://www.w3.org/2000/svg" width="100" height="50">
            <rect width="100" height="50" fill="red"/></svg>"#;
        Arc::new(
            usvg::Tree::from_str(source, &usvg::Options::default())
                .expect("test premise: the SVG parses"),
        )
    }

    fn block_id(n: u64) -> BlockId {
        use std::sync::OnceLock;
        static IDS: OnceLock<(ContextId, PrincipalId)> = OnceLock::new();
        let (ctx, prin) = IDS.get_or_init(|| (ContextId::new(), PrincipalId::new()));
        BlockId::new(*ctx, *prin, n)
    }

    /// The staleness rule, both halves: a re-raster is owed when the physical
    /// size moves (a monitor swap the logical layout never notices) and when
    /// the content moves — and owed to nobody otherwise.
    #[test]
    fn a_raster_is_current_only_for_its_own_version_and_physical_size() {
        let mut cache = SvgRasterCache::default();
        let id = block_id(1);
        cache.insert(
            id,
            SvgRaster {
                content_version: 7,
                physical: (200, 100),
                draw: (200.0, 100.0),
                image: Handle::default(),
            },
        );

        assert!(cache.is_current(&id, 7, (200, 100)));
        assert!(
            !cache.is_current(&id, 7, (400, 200)),
            "a DPI change moves the physical size and nothing else — it must re-raster",
        );
        assert!(!cache.is_current(&id, 8, (200, 100)), "new content re-rasters");
        assert!(!cache.is_current(&block_id(2), 7, (200, 100)));
    }

    /// The cache has no generation counter — quads are assembled and
    /// uploaded every frame, so replacement/drop reach the GPU on their own
    /// (the WindowKey coupling was deleted per review, 2026-08-18). What
    /// still matters is that retain actually drops entries.
    #[test]
    fn retain_drops_rejected_entries() {
        let mut cache = SvgRasterCache::default();
        cache.insert(
            block_id(1),
            SvgRaster {
                content_version: 1,
                physical: (10, 10),
                draw: (10.0, 10.0),
                image: Handle::default(),
            },
        );
        cache.retain(|_| false);
        assert!(cache.is_empty());
    }

    /// Shrink-only fitting, and the physical target that follows from it: a
    /// small SVG keeps its intrinsic size, and the bitmap is sized in
    /// PHYSICAL pixels for the display it lands on.
    #[test]
    fn svg_raster_target_is_physical_and_shrink_only() {
        let (target, scale, draw) = svg_raster_target(100.0, 50.0, 400.0, 2.0, 8192).unwrap();
        assert_eq!(draw, (100.0, 50.0), "a small SVG must not be blown up");
        assert_eq!(target, (200, 100), "the bitmap is sized in physical px");
        assert!((scale - 2.0).abs() < 1e-6, "user units → physical px in one pass");

        let (_, _, shrunk) = svg_raster_target(1000.0, 500.0, 400.0, 1.0, 8192).unwrap();
        assert!((shrunk.0 - 400.0).abs() < 1e-3, "an oversized SVG shrinks to the column");
        assert!((shrunk.1 - 200.0).abs() < 1e-3, "aspect ratio is preserved");
    }

    #[test]
    fn a_degenerate_column_rasterizes_nothing() {
        assert!(svg_raster_target(100.0, 100.0, 0.0, 1.0, 8192).is_none());
        assert!(svg_draw_size(0.0, 100.0, 400.0).is_none());
    }
}
