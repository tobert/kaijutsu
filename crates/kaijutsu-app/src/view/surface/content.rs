//! Formatted block text, keyed by `BlockId` instead of by entity.
//!
//! [`BlockContentCache`] is the entity-free replacement for the pair
//! `sync_block_cell_buffers` + `BlockScene` (`view/render.rs`): the same
//! `format_single_block` / `block_color` / rich-detection flow, but the answer
//! lands in a resource keyed by block id rather than on a spawned `BlockCell`
//! entity. That is the whole point — the surface has no per-block entities to
//! hang a `BlockScene` on.
//!
//! **No streaming debounce here** (slice 3). The legacy path suppresses
//! re-formatting a large `Running` block that grew by fewer than 200 bytes,
//! because on that path a re-format is a whole-block re-shape and a whole-block
//! re-raster. On the surface path a content bump re-shapes the block's *open
//! tail chunk* and nothing else (`super::shape_cache::incremental_prefix`), so
//! the debounce would buy nothing and cost the one thing it was never worth:
//! a streamed reply that visibly lags its own text. The legacy debounce stays
//! where it is, guarding the path that still needs it.
//!
//! Rich content is detected **and kept** (slice 4). Detection is the
//! expensive half — a markdown parse, a diff parse, a usvg tree, an ABC
//! tune — so its answer is cached here behind an `Arc` alongside the text it
//! describes ([`FormattedBlock::rich_content`]), and the shaper reads the
//! payload rather than re-deriving it. Each kind also decides what *text*
//! the block shapes: markdown and Output shape their `plain_text` with span
//! brushes, a diff shapes its preview's `plain_text`, the drawn-only kinds
//! (ABC, SVG, sparkline) shape no text at all, and an image shapes its
//! placeholder label. Everything that isn't glyphs — staff lines, diff
//! bands, the sparkline plot, the placeholder rect — becomes geometry in
//! `super::shape_cache`.

use std::collections::HashMap;

use bevy::prelude::*;
use kaijutsu_types::{BlockId, Status};

use crate::cell::block_border::BorderInputs;
use crate::cell::{CellEditor, ConversationScrollState, EditorEntities, MainCell};
use crate::text::rich::{RichContentKind, SpanBrush};
use crate::ui::theme::Theme;
use crate::view::format::{block_color, format_single_block};
use crate::view::geometry::{ConversationGeometry, RowKey};

/// How many screens of slack, on each side of the viewport, keep a block's
/// text formatted.
///
/// Deliberately more generous than the window the shaper works on: formatting
/// is the cheap half (a string build), and having the text ready before the
/// shaper asks for it keeps a fast scroll from paying format+shape in the
/// same frame.
pub const CONTENT_SLACK_SCREENS: f32 = 2.0;

/// Which rich renderer a block uses — the routing tag, cheap to compare.
///
/// The payload it names lives in [`FormattedBlock::rich_content`]; this is
/// what the change gate and the shaper branch on without touching an `Arc`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RichKindInfo {
    Markdown,
    Sparkline,
    Svg,
    Abc,
    Diff,
    Output,
    Image,
}

impl RichKindInfo {
    fn of(kind: &RichContentKind) -> Self {
        match kind {
            RichContentKind::Markdown { .. } => Self::Markdown,
            RichContentKind::Sparkline(_) => Self::Sparkline,
            RichContentKind::Svg { .. } => Self::Svg,
            RichContentKind::Abc { .. } => Self::Abc,
            RichContentKind::Diff(_) => Self::Diff,
            RichContentKind::Output { .. } => Self::Output,
            RichContentKind::Image { .. } => Self::Image,
        }
    }

    /// Does this kind draw something parley cannot — geometry, a raster, or
    /// a placeholder rect?
    ///
    /// The dividing line the shaper cares about. Markdown and Output are
    /// rich only in their *coloring*: they shape like any other text and take
    /// every fast path (chunking, the incremental tail, the async backlog).
    /// The kinds below build their drawing from theme colors and engraving
    /// machinery on the main thread, in one chunk, and take none of them —
    /// see `shape_cache::shape_rich_block`.
    pub fn is_drawn(self) -> bool {
        matches!(
            self,
            Self::Sparkline | Self::Svg | Self::Abc | Self::Diff | Self::Image
        )
    }
}

/// A shared, cheaply cloned handle on one block's detected rich content.
///
/// A newtype only so it can carry a `Debug` impl: [`RichContentKind`] holds a
/// `usvg::Tree` and an ABC AST, neither of which derives `Debug`, and teaching
/// them to would be a lot of noise for a line nobody prints. The cache above
/// stays `Debug` — which diagnostics do use — and says the honest thing about
/// this field: it is there, and its contents are not printable.
#[derive(Clone)]
pub struct RichPayload(std::sync::Arc<RichContentKind>);

impl RichPayload {
    pub fn kind(&self) -> &RichContentKind {
        &self.0
    }
}

impl std::fmt::Debug for RichPayload {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RichPayload(..)")
    }
}

/// One block's display text and everything the shaper needs to lay it out.
#[derive(Debug, Clone)]
pub struct FormattedBlock {
    /// Document version at the moment this block's *shaped form* last
    /// actually changed.
    ///
    /// Not simply "the version we last looked at it": re-formatting an
    /// unchanged block on somebody else's version bump must not invalidate its
    /// shaped glyphs, so the stamp only moves when something parley would lay
    /// out differently moved with it — the text, the span structure, or the
    /// rich kind. This is what `ShapeKey.content_version` compares.
    ///
    /// **`color` is deliberately not in it.** A color change is not a shaping
    /// event: `shape_cache::recolor_block` repaints cached glyphs in place
    /// without touching a position. Putting color here would re-shape the
    /// whole conversation on a theme swap — exactly the full-document rebake
    /// the rewrite exists to delete. Spans *are* in it, because a span change
    /// is the one color change that cannot be repainted in place (see
    /// `recolor_block`).
    pub version: u64,
    /// The text to shape — `format_single_block`'s output, or markdown's
    /// `plain_text` where detection produced spans for it.
    pub text: String,
    /// Base text color (`block_color`).
    pub color: Color,
    /// Per-byte-range brushes for the text above. Empty for everything but
    /// markdown, Output and diff.
    pub spans: Vec<SpanBrush>,
    /// The block's ANSI style spans, resolved against this theme
    /// (`text::ansi::resolve_style_spans`).
    ///
    /// A separate list from `spans` and not a merge of the two, because they
    /// are different currencies: `spans` is a `peniko::Brush` per range, and
    /// these carry a style-table index, an MSDF weight and their own geometry
    /// (backgrounds, underlines). Nothing produces both today — ANSI spans
    /// come from shell output that rich detection leaves alone — and where
    /// they do meet, `shape_cache::shape_chunk` prefers these, because they
    /// are the currency that can express the other's coloring and not the
    /// reverse.
    ///
    /// **Not honoured by the drawn rich kinds.** A block that detects as ABC,
    /// SVG, a sparkline, a diff or an image shapes through
    /// `super::rich::RichShaper`, which builds its own geometry and never sees
    /// this list — ANSI on such a block is dropped. That combination does not
    /// arise from the ingest hooks today (shell output is plain), and giving
    /// the rich shaper a second span currency is deferred until it does.
    pub style_spans: Vec<crate::text::ansi::StyledSpan>,
    /// Which rich renderer this block wants.
    pub rich: Option<RichKindInfo>,
    /// The detected payload the renderer draws from — the parsed diff, the
    /// engraved tune, the usvg tree, the sparkline series.
    ///
    /// `Arc` because detection is expensive and the reuse gate below hands
    /// the same answer back frame after frame: cloning a pointer is what
    /// makes "detect once" true in the data as well as in the comment. It is
    /// deliberately **not** part of `version` on its own — a payload only
    /// moves when `fingerprint` moves, which is what the change gate checks.
    pub rich_content: Option<RichPayload>,
    /// Last seen block status.
    ///
    /// Two readers. The content sweep uses it to keep refreshing a streaming
    /// block that has scrolled out of the band (its height would otherwise
    /// freeze mid-stream), and `super::shape_cache` uses it to keep a
    /// `Running` block off the async path and on the incremental tail.
    pub status: Status,
    /// The border decision's inputs, projected off the same snapshot the text
    /// came from ([`BorderInputs`]).
    ///
    /// Deliberately **outside** `version`: status, error and exclusion move
    /// without changing a glyph, and restamping the content version for them
    /// would re-shape the block every time a tool call finished. Chrome reads
    /// this and keeps its own version (`super::chrome`).
    pub border: BorderInputs,
    /// `rich_input_fingerprint` of the inputs detection last ran on, so a
    /// streaming neighbour's version bump doesn't drag every block through a
    /// re-parse (same gate as `view/render.rs:152-160`).
    fingerprint: u64,
    /// [`super::shape_cache::SurfaceThemeEpoch`] at the last detection run.
    ///
    /// Part of the reuse gate alongside `fingerprint`: markdown's span brushes
    /// are built from **theme** colors, and the fingerprint only covers the
    /// block's own inputs — so without this a theme swap would silently keep
    /// the old palette on every markdown block until its text next changed.
    theme_epoch: u64,
    /// Fingerprint of `spans`, so a span-only change (a theme swap repainting
    /// markdown) can move `version` without comparing brush lists.
    spans_fingerprint: u64,
}

/// Order-sensitive fingerprint of a block's two span lists.
///
/// `SpanBrush` has no `PartialEq` (peniko brushes carry gradients and images),
/// and the only question asked of it here is "did the coloring move", so a
/// hash of the resolved RGBA is both sufficient and cheap.
///
/// The styled half is hashed field-by-field
/// (`text::ansi::styled_spans_fingerprint`) rather than by color alone,
/// because an ANSI span carries things a color cannot express: which palette
/// slot the GPU will resolve, how heavy the stem is, whether there is a
/// background quad or an underline under it. An attribute that moves without
/// moving this hash is a block that keeps its old shaping — silently, since
/// nothing downstream re-checks.
fn spans_fingerprint(
    spans: &[SpanBrush],
    style_spans: &[crate::text::ansi::StyledSpan],
) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    spans.len().hash(&mut hasher);
    for span in spans {
        span.start.hash(&mut hasher);
        span.end.hash(&mut hasher);
        crate::text::msdf::layout_bridge::brush_to_rgba8(&span.brush).hash(&mut hasher);
    }
    crate::text::ansi::styled_spans_fingerprint(style_spans, &mut hasher);
    hasher.finish()
}

/// Formatted display text for every block the surface might draw.
///
/// **Not band-bounded**: entries are added for the content band and only
/// removed when a block leaves the *document* ([`BlockContentCache::retain_ids`]),
/// so this grows with how far the user has scrolled. Consumers must iterate a
/// band of geometry rows and `get` each one — never iterate the cache — or
/// they inherit that growth as a per-frame cost (`super::chrome` did, briefly;
/// kaibo/deepseek review, 2026-08-18).
///
/// Slice 3 bounded the *shaped* cache (`shape_cache::SHAPE_CACHE_MAX_GLYPHS`)
/// and deliberately not this one: glyphs are ~32 bytes each and dominate,
/// while this holds strings the block store already holds. Bounding it is
/// open work, tracked in `docs/issues.md` — the eviction rule would be the
/// same pinned-window LRU, and the reason it isn't shared is that the two
/// caches are wanted at different bands.
#[derive(Resource, Debug, Default)]
pub struct BlockContentCache {
    blocks: HashMap<BlockId, FormattedBlock>,
}

impl BlockContentCache {
    pub fn get(&self, id: &BlockId) -> Option<&FormattedBlock> {
        self.blocks.get(id)
    }

    pub fn len(&self) -> usize {
        self.blocks.len()
    }

    #[allow(dead_code)] // Symmetry with `len`; the extractor will want it.
    pub fn is_empty(&self) -> bool {
        self.blocks.is_empty()
    }

    /// Ids whose last known status was `Running`. These are refreshed
    /// wherever they sit in the document — a streaming block scrolled out of
    /// the band still has to reach its final text, or its measured height
    /// freezes mid-stream.
    fn running_ids(&self) -> Vec<BlockId> {
        self.blocks
            .iter()
            .filter(|(_, fb)| fb.status == Status::Running)
            .map(|(id, _)| *id)
            .collect()
    }

    /// Seed a formatted block directly. Test-only: `super::chrome`'s tests
    /// need a populated content cache without a block store or a font
    /// context, and `fingerprint` is private to this module.
    #[cfg(test)]
    pub(crate) fn insert_for_test(&mut self, id: BlockId, border: BorderInputs) {
        self.blocks.insert(
            id,
            FormattedBlock {
                version: 1,
                text: String::new(),
                color: Color::WHITE,
                spans: Vec::new(),
                style_spans: Vec::new(),
                rich: None,
                rich_content: None,
                status: border.status,
                border,
                fingerprint: 0,
                theme_epoch: 0,
                spans_fingerprint: 0,
            },
        );
    }

    /// Drop everything the document no longer contains.
    fn retain_ids(&mut self, live: &std::collections::HashSet<BlockId>) {
        self.blocks.retain(|id, _| live.contains(id));
    }
}

/// Format in-window (and streaming) blocks into [`BlockContentCache`].
///
/// The band is `visible_rows` with [`CONTENT_SLACK_SCREENS`] of slack on each
/// side, plus every block the cache last saw `Running`. A block whose
/// snapshot has vanished is dropped rather than left stale.
#[allow(clippy::too_many_arguments)]
pub fn sync_block_content(
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    geometries: Query<&ConversationGeometry>,
    scroll_state: Res<ConversationScrollState>,
    theme: Res<Theme>,
    theme_epoch: Res<super::shape_cache::SurfaceThemeEpoch>,
    svg_fontdb: Res<crate::text::SvgFontDb>,
    doc_cache: Res<crate::cell::DocumentCache>,
    mut cache: ResMut<BlockContentCache>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(editor) = main_cells.get(main_ent) else {
        return;
    };
    let Ok(geom) = geometries.get(main_ent) else {
        return;
    };

    let doc_version = editor.version();
    let vh = if scroll_state.visible_height > 0.0 {
        scroll_state.visible_height
    } else {
        600.0
    };
    let band = geom.visible_rows(scroll_state.offset, vh, CONTENT_SLACK_SCREENS * vh);

    let mut wanted: Vec<BlockId> = Vec::with_capacity(band.len());
    for row in &geom.rows()[band] {
        if let RowKey::Block(id) = row.key {
            wanted.push(id);
        }
    }
    for id in cache.running_ids() {
        if !wanted.contains(&id) {
            wanted.push(id);
        }
    }

    let local_ctx = doc_cache.active_id();
    let epoch = theme_epoch.get();
    // Built once per sweep, not once per block: the 6×6×6 cube and the
    // grayscale ramp are arithmetic, and re-deriving them per block would be
    // 256 slots of it for every styled row on screen.
    let palette = theme.ansi.palette_256();
    // What an inverted span's glyphs are painted with when the span itself
    // names no background — the surface behind the text, which is what a
    // terminal reverses against.
    let default_bg = crate::text::color_to_rgba8(theme.bg);

    for id in wanted {
        let Some(block) = editor.block_snapshot(&id) else {
            // Raced a removal — the id lives in a geometry row that the next
            // reconcile will drop. Nothing to cache.
            cache.blocks.remove(&id);
            continue;
        };

        let text = format_single_block(&block, local_ctx, &|pid| editor.block_snapshot(pid));

        let color = block_color(&block, &theme);
        let fingerprint = crate::text::rich::rich_input_fingerprint(&text, &block);

        // Detection is the expensive half. Reuse the previous answer when the
        // inputs it reads haven't moved — the same gate `sync_block_cell_buffers`
        // uses, and for the same reason: `doc_version` is a whole-document
        // counter, so one streaming block would otherwise drag every in-band
        // block through a re-parse every frame. The theme epoch joins the gate
        // because markdown's spans are built from theme colors, which the
        // block-input fingerprint knows nothing about.
        let reuse_detection = cache
            .blocks
            .get(&id)
            .is_some_and(|prev| prev.fingerprint == fingerprint && prev.theme_epoch == epoch);

        let detected = if reuse_detection {
            let prev = &cache.blocks[&id];
            // The previous answer was derived from exactly these inputs, so
            // it still describes this text — right down to the payload, which
            // comes across as a pointer rather than a re-parse.
            Detected {
                text: prev.text.clone(),
                spans: prev.spans.clone(),
                rich: prev.rich,
                content: prev.rich_content.clone(),
            }
        } else {
            detect(&block, text, &theme, &svg_fontdb)
        };
        let Detected {
            text,
            spans,
            rich,
            content: rich_content,
        } = detected;

        // Deliberately **outside** the detection reuse gate above. ANSI spans
        // are not something detection derives — they arrive on the snapshot,
        // already parsed by the kernel — so `rich_input_fingerprint` (which
        // documents itself as exactly `detect_rich_content_typed`'s input set)
        // stays honest about its own inputs, and staleness is prevented by
        // simply re-resolving these every sweep. Cheap: the list is empty for
        // every block that did not come through an ingest transform, and a
        // resolved span is a handful of integer moves.
        let style_spans = crate::text::ansi::resolve_style_spans(
            &block.style_spans,
            crate::text::color_to_rgba8(color),
            default_bg,
            &palette,
        );
        let spans_fp = spans_fingerprint(&spans, &style_spans);

        // Only shaping-relevant movement restamps the version — colors are the
        // shaper's fast path, not its trigger (see `FormattedBlock::version`).
        //
        // The fingerprint clause is what covers the *drawn* kinds: an ABC
        // tune, an SVG and a sparkline all shape the empty string, so their
        // text, spans and tag are constant while the thing actually on screen
        // changed completely. `fingerprint` is the hash of the inputs
        // detection ran on, so it moves exactly when the payload does.
        let changed = match cache.blocks.get(&id) {
            Some(prev) => {
                prev.text != text
                    || prev.rich != rich
                    || prev.spans_fingerprint != spans_fp
                    || (rich.is_some_and(RichKindInfo::is_drawn) && prev.fingerprint != fingerprint)
            }
            None => true,
        };
        let version = match cache.blocks.get(&id) {
            Some(prev) if !changed => prev.version,
            _ => doc_version,
        };

        cache.blocks.insert(
            id,
            FormattedBlock {
                version,
                text,
                color,
                spans,
                style_spans,
                rich,
                rich_content,
                status: block.status,
                border: BorderInputs::from_snapshot(&block),
                fingerprint,
                theme_epoch: epoch,
                spans_fingerprint: spans_fp,
            },
        );
    }

    // Blocks that left the document (context switch, exclusion, truncation)
    // must not linger — they'd hold text and, downstream, shaped glyphs for a
    // document nobody is looking at any more.
    let live: std::collections::HashSet<BlockId> = editor.block_ids().into_iter().collect();
    if cache.len() > live.len() {
        cache.retain_ids(&live);
    }
}

/// What detection decided: the text to shape, its span brushes, the routing
/// tag, and the payload the drawn kinds build from.
struct Detected {
    text: String,
    spans: Vec<SpanBrush>,
    rich: Option<RichKindInfo>,
    content: Option<RichPayload>,
}

/// Run rich detection on one block and split the answer into the two halves
/// the surface draws with: text parley shapes, and a payload the geometry /
/// quad lanes build from.
///
/// Which text a kind shapes is a per-kind decision and it is the whole of the
/// routing:
///
/// - **Markdown** shapes its `plain_text` (the source stripped of syntax)
///   with span brushes — unchanged since slice 1.
/// - **Output** shapes its whitespace-padded `plain_text` with per-cell
///   brushes, matching `build_block_scenes`' Output arm.
/// - **Diff** shapes the preview's `plain_text` with the diff span brushes,
///   because word-level highlights start *inside* a line and only parley's
///   ranged brushes split a shaping run there. Its bands and washes are
///   geometry.
/// - **ABC / SVG / sparkline** shape **nothing** — they have no text of their
///   own, exactly as the legacy arms clear `msdf_glyphs` for them. Drawing
///   them is entirely geometry (ABC's own glyphs come from the engraving IR,
///   not from parley) or a raster.
/// - **Image** shapes its `[image: …]` placeholder label; the dark rectangle
///   under it is geometry.
fn detect(
    block: &kaijutsu_types::BlockSnapshot,
    text: String,
    theme: &Theme,
    svg_fontdb: &crate::text::SvgFontDb,
) -> Detected {
    use kaijutsu_types::{BlockKind, ContentType, Role};

    let is_rich_candidate =
        block.kind == BlockKind::Text && matches!(block.role, Role::Model | Role::Tool);
    let is_typed_result =
        block.kind == BlockKind::ToolResult && block.content_type != ContentType::Plain;
    let is_output_candidate =
        block.kind == BlockKind::ToolResult && block.output.is_some() && !block.is_error;

    let streaming = block.status == Status::Running;
    let detected = if is_rich_candidate || is_typed_result {
        crate::text::rich::detect_rich_content_typed(
            &text,
            block.content_type,
            Some(svg_fontdb),
            streaming,
        )
    } else {
        None
    };
    let detected = detected.or_else(|| {
        if is_output_candidate {
            block
                .output
                .as_ref()
                .and_then(crate::text::rich::detect_output_content)
        } else {
            None
        }
    });

    let Some(rich) = detected else {
        return Detected {
            text,
            spans: Vec::new(),
            rich: None,
            content: None,
        };
    };

    let info = RichKindInfo::of(&rich.kind);
    let kind = RichPayload(std::sync::Arc::new(rich.kind));
    let (text, spans) = match kind.kind() {
        RichContentKind::Markdown { spans, plain_text } => {
            let md_colors = crate::text::markdown::MarkdownColors {
                heading: theme.md_heading_color,
                code: theme.md_code_fg,
                strong: theme.md_strong_color,
                code_block: theme.md_code_block_fg,
            };
            let brushes =
                crate::text::rich::build_span_brushes(spans, theme.block_assistant, &md_colors);
            (plain_text.clone(), brushes)
        }
        RichContentKind::Output { layout, plain_text } => (
            plain_text.clone(),
            crate::text::rich::build_output_span_brushes(layout, theme),
        ),
        RichContentKind::Diff(preview) => (
            preview.plain_text.clone(),
            crate::text::rich::build_diff_span_brushes(preview, theme),
        ),
        // No text of their own — the drawing is geometry or a raster.
        RichContentKind::Abc { .. }
        | RichContentKind::Svg { .. }
        | RichContentKind::Sparkline(_) => (String::new(), Vec::new()),
        RichContentKind::Image { hash } => (image_placeholder_label(hash), Vec::new()),
    };

    Detected {
        text,
        spans,
        rich: Some(info),
        content: Some(kind),
    }
}

/// The placeholder caption an `Image` block draws until there is a
/// CAS→decode pipeline behind it. Verbatim from `build_block_scenes`' Image
/// arm, which is the point — the two paths must not disagree about what an
/// undecoded image looks like.
pub fn image_placeholder_label(hash: &str) -> String {
    format!("[image: {}]", &hash[..8.min(hash.len())])
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockKind, ContentType, Role};

    /// A headless `App` running only `sync_block_content`, with just the
    /// resources it touches. No rendering plugins — formatting is pure.
    fn content_app() -> App {
        let mut app = App::new();
        app.init_resource::<EditorEntities>();
        app.init_resource::<crate::text::TextMetrics>();
        app.init_resource::<Theme>();
        app.init_resource::<crate::text::SvgFontDb>();
        app.init_resource::<crate::cell::DocumentCache>();
        app.init_resource::<ConversationScrollState>();
        app.init_resource::<BlockContentCache>();
        app.init_resource::<crate::view::surface::shape_cache::SurfaceThemeEpoch>();
        app.add_systems(
            Update,
            (
                crate::view::geometry::sync_conversation_geometry,
                crate::view::surface::shape_cache::track_surface_theme_epoch,
                sync_block_content,
            )
                .chain(),
        );
        app
    }

    fn seed(app: &mut App, texts: &[&str]) -> Vec<BlockId> {
        let mut editor = CellEditor::new();
        let mut ids = Vec::new();
        for text in texts {
            let id = editor
                .store
                .insert_block(
                    None,
                    ids.last(),
                    Role::User,
                    BlockKind::Text,
                    *text,
                    Status::Done,
                    ContentType::Plain,
                )
                .expect("insert_block");
            ids.push(id);
        }
        let main_ent = app
            .world_mut()
            .spawn((editor, MainCell, ConversationGeometry::default()))
            .id();
        app.world_mut()
            .resource_mut::<EditorEntities>()
            .main_cell = Some(main_ent);
        ids
    }

    fn cache(app: &App) -> &BlockContentCache {
        app.world().resource::<BlockContentCache>()
    }

    /// Replace one block's content and advance the document version.
    ///
    /// `RenderBlockStore` has no in-place text mutation — the app rebuilds it
    /// wholesale from change-feed snapshots (`sync_main_cell_to_conversation`)
    /// — so tests edit the same way rather than inventing a mutation API the
    /// production path doesn't have.
    fn edit_block(app: &mut App, id: BlockId, content: &str) {
        let main_ent = app.world().resource::<EditorEntities>().main_cell.unwrap();
        let mut editor = app.world_mut().get_mut::<CellEditor>(main_ent).unwrap();
        let version = editor.version();
        let principal = editor.store.principal_id();
        let mut snapshots = editor.blocks();
        for snapshot in &mut snapshots {
            if snapshot.id == id {
                snapshot.content = content.to_string();
            }
        }
        let mut store = editor
            .store
            .rebuild(id.context_id, principal, snapshots.iter(), |_| false)
            .expect("rebuild");
        store.set_version(version + 1);
        editor.store = store;
    }

    #[test]
    fn in_band_blocks_are_formatted() {
        let mut app = content_app();
        let ids = seed(&mut app, &["hello", "world"]);
        app.update();
        assert_eq!(cache(&app).get(&ids[0]).unwrap().text, "hello");
        assert_eq!(cache(&app).get(&ids[1]).unwrap().text, "world");
    }

    /// The gate that makes `ShapeKey.content_version` worth having: a
    /// document version bump that leaves a block's rendered form alone must
    /// not restamp it, or every block re-shapes whenever any block streams.
    #[test]
    fn an_unrelated_version_bump_does_not_restamp_a_block() {
        let mut app = content_app();
        let ids = seed(&mut app, &["hello", "world"]);
        app.update();
        let before = cache(&app).get(&ids[0]).unwrap().version;

        // Touch the *other* block, bumping the document version.
        edit_block(&mut app, ids[1], "world again");
        app.update();

        assert_eq!(
            cache(&app).get(&ids[0]).unwrap().version,
            before,
            "an untouched block must keep its content stamp",
        );
        assert!(
            cache(&app).get(&ids[1]).unwrap().version > before,
            "the edited block must restamp",
        );
        assert_eq!(cache(&app).get(&ids[1]).unwrap().text, "world again");
    }

    #[test]
    fn a_removed_block_is_evicted_from_the_cache() {
        let mut app = content_app();
        let ids = seed(&mut app, &["hello", "world"]);
        app.update();
        assert!(cache(&app).get(&ids[1]).is_some());

        // Rebuild the store without the second block — the shape a context
        // switch or a truncation takes on this path.
        let main_ent = app.world().resource::<EditorEntities>().main_cell.unwrap();
        {
            let mut editor = app.world_mut().get_mut::<CellEditor>(main_ent).unwrap();
            let version = editor.version();
            let principal = editor.store.principal_id();
            let snapshots = editor.blocks();
            let dropped = ids[1];
            let mut store = editor
                .store
                .rebuild(ids[0].context_id, principal, snapshots.iter(), |b| {
                    b.id == dropped
                })
                .expect("rebuild");
            store.set_version(version + 1);
            editor.store = store;
        }
        app.update();

        assert!(
            cache(&app).get(&ids[1]).is_none(),
            "a block that left the document must not keep its formatted text",
        );
        assert!(cache(&app).get(&ids[0]).is_some());
    }

    /// The debounce is **gone** on this path, and this is the test that says
    /// so: a 10-character append to a 10 KB `Running` block lands the same
    /// tick. On the legacy path the same edit is suppressed, because there a
    /// re-format means re-shaping and re-rastering the whole block; here it
    /// means re-shaping one chunk. A streamed reply that visibly lags its own
    /// text was the price of the old rule, and nothing pays it now.
    #[test]
    fn a_small_append_to_a_large_streaming_block_lands_immediately() {
        let mut app = content_app();
        let big = "x".repeat(10_000 + 100);
        let mut editor = CellEditor::new();
        let id = editor
            .store
            .insert_block(
                None,
                None,
                Role::Model,
                BlockKind::Thinking,
                &big,
                Status::Running,
                ContentType::Plain,
            )
            .expect("insert_block");
        let main_ent = app
            .world_mut()
            .spawn((editor, MainCell, ConversationGeometry::default()))
            .id();
        app.world_mut()
            .resource_mut::<EditorEntities>()
            .main_cell = Some(main_ent);
        app.update();
        let first_len = cache(&app).get(&id).unwrap().text.len();
        assert!(first_len > 10_000, "test premise: the block is large");

        let grown = format!("{big}{}", "y".repeat(10));
        edit_block(&mut app, id, &grown);
        app.update();

        assert_eq!(
            cache(&app).get(&id).unwrap().text.len(),
            first_len + 10,
            "a 10-char append must reach the surface the tick it happens",
        );
    }

    /// Seed one block with an explicit role/kind/content-type, for the rich
    /// kinds — detection only looks at a block whose role and kind say it
    /// could be rich.
    fn seed_one(
        app: &mut App,
        role: Role,
        kind: BlockKind,
        content_type: ContentType,
        text: &str,
    ) -> BlockId {
        let mut editor = CellEditor::new();
        let id = editor
            .store
            .insert_block(None, None, role, kind, text, Status::Done, content_type)
            .expect("insert_block");
        let main_ent = app
            .world_mut()
            .spawn((editor, MainCell, ConversationGeometry::default()))
            .id();
        app.world_mut()
            .resource_mut::<EditorEntities>()
            .main_cell = Some(main_ent);
        id
    }

    /// A sparkline shapes NO text — its drawing is entirely geometry — which
    /// is exactly why its version gate cannot be "did the text change".
    /// Without the payload clause a new series would keep the old plot
    /// forever.
    #[test]
    fn a_sparklines_data_change_restamps_the_version_though_its_text_is_empty() {
        let mut app = content_app();
        let id = seed_one(
            &mut app,
            Role::Model,
            BlockKind::Text,
            ContentType::Plain,
            "```sparkline\n1 3 7\n```",
        );
        app.update();

        let first = cache(&app).get(&id).unwrap();
        assert_eq!(first.rich, Some(RichKindInfo::Sparkline));
        assert!(first.text.is_empty(), "a sparkline has no text of its own");
        assert!(first.rich_content.is_some(), "the parsed series must be kept");
        let before = first.version;

        edit_block(&mut app, id, "```sparkline\n9 9 1\n```");
        app.update();

        assert!(
            cache(&app).get(&id).unwrap().version > before,
            "a new series must invalidate the shaped plot",
        );
    }

    /// A diff shapes its *preview's* plain text with span brushes, not the
    /// raw tool output — the same text and the same brushes the legacy Diff
    /// arm lays out.
    #[test]
    fn a_diff_shapes_its_preview_text_with_spans() {
        let mut app = content_app();
        let diff = "--- a/x.rs\n+++ b/x.rs\n@@ -1 +1 @@\n-old\n+new\n";
        let id = seed_one(
            &mut app,
            Role::Tool,
            BlockKind::ToolResult,
            ContentType::Diff,
            diff,
        );
        app.update();

        let entry = cache(&app).get(&id).unwrap();
        assert_eq!(entry.rich, Some(RichKindInfo::Diff));
        assert!(entry.rich_content.is_some(), "the parsed preview must be kept");
        assert!(!entry.spans.is_empty(), "diff coloring is per-span, not per-block");
        assert!(
            entry.text.contains("new"),
            "the preview's own text is what gets shaped: {:?}",
            entry.text,
        );
    }

    /// The routing rule the shaper branches on. Markdown and Output are rich
    /// only in their coloring and must keep every text fast path; the rest
    /// build drawings the glyph lane cannot express.
    #[test]
    fn only_the_drawing_kinds_are_marked_drawn() {
        assert!(!RichKindInfo::Markdown.is_drawn());
        assert!(!RichKindInfo::Output.is_drawn());
        for kind in [
            RichKindInfo::Sparkline,
            RichKindInfo::Svg,
            RichKindInfo::Abc,
            RichKindInfo::Diff,
            RichKindInfo::Image,
        ] {
            assert!(kind.is_drawn(), "{kind:?} draws something parley cannot");
        }
    }

    // ---- ANSI style spans ------------------------------------------------

    /// Replace one block's ANSI style spans and advance the document version —
    /// the shape a `SpansChanged` fold (or a `kj block reproject`) takes once
    /// it reaches the app's mirror.
    fn set_style_spans(app: &mut App, id: BlockId, spans: Vec<kaijutsu_types::StyleSpan>) {
        let main_ent = app.world().resource::<EditorEntities>().main_cell.unwrap();
        let mut editor = app.world_mut().get_mut::<CellEditor>(main_ent).unwrap();
        let version = editor.version();
        let principal = editor.store.principal_id();
        let mut snapshots = editor.blocks();
        for snapshot in &mut snapshots {
            if snapshot.id == id {
                snapshot.style_spans = spans.clone();
            }
        }
        let mut store = editor
            .store
            .rebuild(id.context_id, principal, snapshots.iter(), |_| false)
            .expect("rebuild");
        store.set_version(version + 1);
        editor.store = store;
    }

    fn ansi_span(start: u32, end: u32) -> kaijutsu_types::StyleSpan {
        kaijutsu_types::StyleSpan {
            start,
            end,
            fg: None,
            bg: None,
            attrs: kaijutsu_types::StyleAttrs::default(),
        }
    }

    /// A block with no ingest transform behind it carries no styled spans, and
    /// that emptiness is what keeps it on the plain shaping path (and out of
    /// the theme-epoch `ShapeKey`).
    #[test]
    fn an_ordinary_block_has_no_style_spans() {
        let mut app = content_app();
        let ids = seed(&mut app, &["hello"]);
        app.update();
        assert!(cache(&app).get(&ids[0]).unwrap().style_spans.is_empty());
    }

    /// Spans arriving on a block whose text never changed must restamp its
    /// version — the whole point of `SpansChanged` and of `kj block reproject`
    /// is that the colors show up without an edit.
    #[test]
    fn spans_arriving_on_unchanged_text_restamp_the_version() {
        let mut app = content_app();
        let ids = seed(&mut app, &["hello"]);
        app.update();
        let before = cache(&app).get(&ids[0]).unwrap().version;

        set_style_spans(
            &mut app,
            ids[0],
            vec![kaijutsu_types::StyleSpan {
                fg: Some(kaijutsu_types::StyleColor::Indexed(1)),
                ..ansi_span(0, 5)
            }],
        );
        app.update();

        let after = cache(&app).get(&ids[0]).unwrap();
        assert_eq!(after.text, "hello", "test premise: the text did not move");
        assert_eq!(after.style_spans.len(), 1);
        assert!(
            after.version > before,
            "a span-only change must invalidate the shaped glyphs",
        );
    }

    /// The trap this closes: an attribute change that leaves every *color*
    /// alone. Underline is drawn in the geometry lane, so a fingerprint that
    /// hashed colors only would leave the block shaped without it, forever.
    #[test]
    fn an_underline_only_span_change_restamps_the_version() {
        let mut app = content_app();
        let ids = seed(&mut app, &["hello"]);
        let red = kaijutsu_types::StyleSpan {
            fg: Some(kaijutsu_types::StyleColor::Indexed(1)),
            ..ansi_span(0, 5)
        };
        set_style_spans(&mut app, ids[0], vec![red]);
        app.update();
        let before = cache(&app).get(&ids[0]).unwrap().version;

        set_style_spans(
            &mut app,
            ids[0],
            vec![kaijutsu_types::StyleSpan {
                attrs: kaijutsu_types::StyleAttrs::UNDERLINE,
                ..red
            }],
        );
        app.update();

        let after = cache(&app).get(&ids[0]).unwrap();
        assert!(after.style_spans[0].underline, "test premise: underline is on");
        assert!(
            after.version > before,
            "an attribute-only span change must restamp the version",
        );
    }

    /// Same trap, the weight axis: bold changes no color at all — it is an
    /// `importance` on the glyph — so the fingerprint has to see it too.
    #[test]
    fn a_bold_only_span_change_restamps_the_version() {
        let mut app = content_app();
        let ids = seed(&mut app, &["hello"]);
        let red = kaijutsu_types::StyleSpan {
            fg: Some(kaijutsu_types::StyleColor::Indexed(1)),
            ..ansi_span(0, 5)
        };
        set_style_spans(&mut app, ids[0], vec![red]);
        app.update();
        let before = cache(&app).get(&ids[0]).unwrap().version;

        set_style_spans(
            &mut app,
            ids[0],
            vec![kaijutsu_types::StyleSpan {
                attrs: kaijutsu_types::StyleAttrs::BOLD,
                ..red
            }],
        );
        app.update();

        assert!(
            cache(&app).get(&ids[0]).unwrap().version > before,
            "a weight-only span change must restamp the version",
        );
    }

    /// A color-only change must NOT restamp the content version: colors are
    /// repainted in place by `shape_cache::recolor_block`, and restamping
    /// would re-shape the whole conversation on a theme swap — the very
    /// full-document rebake this rewrite deletes.
    #[test]
    fn a_color_change_alone_does_not_restamp_the_version() {
        let mut app = content_app();
        let ids = seed(&mut app, &["hello"]);
        app.update();
        let before = cache(&app).get(&ids[0]).unwrap().version;
        let color_before = cache(&app).get(&ids[0]).unwrap().color;

        // Repaint the role this block draws in, and bump the document version
        // so the sweep genuinely re-derives everything.
        app.world_mut().resource_mut::<Theme>().block_user = Color::srgb(1.0, 0.0, 1.0);
        edit_block(&mut app, ids[0], "hello");
        app.update();

        let after = cache(&app).get(&ids[0]).unwrap();
        assert_ne!(after.color, color_before, "test premise: the color moved");
        assert_eq!(
            after.version, before,
            "a color-only change must not invalidate shaped glyphs",
        );
    }
}
