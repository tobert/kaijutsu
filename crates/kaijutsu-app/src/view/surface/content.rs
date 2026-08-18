//! Formatted block text, keyed by `BlockId` instead of by entity.
//!
//! [`BlockContentCache`] is the entity-free replacement for the pair
//! `sync_block_cell_buffers` + `BlockScene` (`view/render.rs`): the same
//! `format_single_block` / `block_color` / rich-detection flow, the same
//! streaming debounce, but the answer lands in a resource keyed by block id
//! rather than on a spawned `BlockCell` entity. That is the whole point —
//! the surface has no per-block entities to hang a `BlockScene` on.
//!
//! Rich content is **detected but not built** in this slice: the kind is
//! recorded on [`FormattedBlock::rich`] so slice 4 can route ABC / SVG /
//! sparkline / diff into the surface's geometry lane, and markdown's span
//! brushes are kept (they fall straight out of detection and
//! `layout_spanned` wants them), but everything else renders as its plain
//! formatted text for now. That is the "known-accepted in slice 1" line from
//! the plan, made explicit in the data rather than left to the renderer to
//! discover.

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

/// Debounce thresholds for a large *streaming* block, ported verbatim from
/// `sync_block_cell_buffers` (`view/render.rs:98-108`): while a block is
/// `Running` and already past `DEBOUNCE_MIN_SIZE`, a growth of fewer than
/// `DEBOUNCE_CHARS` bytes doesn't earn a re-format. It dies in slice 3, when
/// the tail chunk re-shapes on its own and re-formatting a huge block stops
/// being the expensive part.
const DEBOUNCE_CHARS: usize = 200;
const DEBOUNCE_MIN_SIZE: usize = 10_000;

/// Which rich renderer a block *would* use, recorded without building it.
///
/// Slice 1 draws every one of these as plain text (markdown excepted — its
/// spans are free). Slice 4 turns this tag into the geometry/quad routing.
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
}

/// One block's display text and everything the shaper needs to lay it out.
#[derive(Debug, Clone)]
pub struct FormattedBlock {
    /// Document version at the moment this block's *rendered form* last
    /// actually changed.
    ///
    /// Not simply "the version we last looked at it": re-formatting an
    /// unchanged block on somebody else's version bump must not invalidate
    /// its shaped glyphs, so the stamp only moves when the text, color or
    /// rich kind moved with it. This is what `ShapeKey.content_version`
    /// compares.
    pub version: u64,
    /// The text to shape — `format_single_block`'s output, or markdown's
    /// `plain_text` where detection produced spans for it.
    pub text: String,
    /// Base text color (`block_color`).
    pub color: Color,
    /// Per-byte-range brushes for the text above. Empty for everything but
    /// markdown today.
    pub spans: Vec<SpanBrush>,
    /// Which rich renderer this block wants, once one exists for it.
    pub rich: Option<RichKindInfo>,
    /// Last seen block status — drives the streaming debounce, and tells the
    /// content sweep which out-of-band blocks still need refreshing.
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
}

/// Formatted display text for every block the surface might draw.
///
/// **Not band-bounded**: entries are added for the content band and only
/// removed when a block leaves the *document* ([`BlockContentCache::retain_ids`]),
/// so this grows with how far the user has scrolled. Consumers must iterate a
/// band of geometry rows and `get` each one — never iterate the cache — or
/// they inherit that growth as a per-frame cost (`super::chrome` did, briefly;
/// kaibo/deepseek review, 2026-08-18). Slice 3's LRU eviction is what finally
/// bounds it.
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
                rich: None,
                status: border.status,
                border,
                fingerprint: 0,
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
pub fn sync_block_content(
    entities: Res<EditorEntities>,
    main_cells: Query<&CellEditor, With<MainCell>>,
    geometries: Query<&ConversationGeometry>,
    scroll_state: Res<ConversationScrollState>,
    theme: Res<Theme>,
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

    for id in wanted {
        let Some(block) = editor.block_snapshot(&id) else {
            // Raced a removal — the id lives in a geometry row that the next
            // reconcile will drop. Nothing to cache.
            cache.blocks.remove(&id);
            continue;
        };

        let text = format_single_block(&block, local_ctx, &|pid| editor.block_snapshot(pid));

        // Streaming debounce: a huge Running block that only grew a little
        // isn't worth re-formatting (and, downstream, re-shaping) this tick.
        if block.status == Status::Running
            && text.len() > DEBOUNCE_MIN_SIZE
            && let Some(prev) = cache.blocks.get(&id)
            && !prev.text.is_empty()
        {
            let growth = text.len().saturating_sub(prev.text.len());
            if growth > 0 && growth < DEBOUNCE_CHARS {
                continue;
            }
        }

        let color = block_color(&block, &theme);
        let fingerprint = crate::text::rich::rich_input_fingerprint(&text, &block);

        // Detection is the expensive half. Reuse the previous answer when the
        // inputs it reads haven't moved — the same gate `sync_block_cell_buffers`
        // uses, and for the same reason: `doc_version` is a whole-document
        // counter, so one streaming block would otherwise drag every in-band
        // block through a re-parse every frame.
        let reuse_detection = cache
            .blocks
            .get(&id)
            .is_some_and(|prev| prev.fingerprint == fingerprint);

        let (text, spans, rich) = if reuse_detection {
            let prev = &cache.blocks[&id];
            // Markdown's `plain_text`/spans were derived from exactly these
            // inputs, so the previous answer still describes this text.
            (prev.text.clone(), prev.spans.clone(), prev.rich)
        } else {
            detect(&block, text, &theme, &svg_fontdb)
        };

        let changed = match cache.blocks.get(&id) {
            Some(prev) => prev.text != text || prev.color != color || prev.rich != rich,
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
                rich,
                status: block.status,
                border: BorderInputs::from_snapshot(&block),
                fingerprint,
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

/// Run rich detection on one block and reduce it to what the surface can use
/// today: the text to shape, its span brushes, and a tag naming the renderer
/// slice 4 will eventually route it to.
///
/// Only markdown changes what gets shaped (its `plain_text` is the source
/// stripped of syntax, and its spans are the coloring). Every other kind
/// falls back to the formatted text — the accepted slice-1 rendering.
fn detect(
    block: &kaijutsu_types::BlockSnapshot,
    text: String,
    theme: &Theme,
    svg_fontdb: &crate::text::SvgFontDb,
) -> (String, Vec<SpanBrush>, Option<RichKindInfo>) {
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
        return (text, Vec::new(), None);
    };

    let info = RichKindInfo::of(&rich.kind);
    match rich.kind {
        RichContentKind::Markdown { spans, plain_text } => {
            let md_colors = crate::text::markdown::MarkdownColors {
                heading: theme.md_heading_color,
                code: theme.md_code_fg,
                strong: theme.md_strong_color,
                code_block: theme.md_code_block_fg,
            };
            let brushes =
                crate::text::rich::build_span_brushes(&spans, theme.block_assistant, &md_colors);
            (plain_text, brushes, Some(info))
        }
        // Everything else renders as its plain formatted text this slice.
        _ => (text, Vec::new(), Some(info)),
    }
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
        app.add_systems(
            Update,
            (
                crate::view::geometry::sync_conversation_geometry,
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

    /// The debounce, at the seam it guards: a huge `Running` block that grew
    /// by a handful of characters keeps its previous text this tick.
    #[test]
    fn a_large_streaming_block_debounces_small_growth() {
        let mut app = content_app();
        let big = "x".repeat(DEBOUNCE_MIN_SIZE + 100);
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
        assert!(first_len > DEBOUNCE_MIN_SIZE, "test premise: block is large");

        let small_growth = format!("{big}{}", "y".repeat(10));
        edit_block(&mut app, id, &small_growth);
        app.update();
        assert_eq!(
            cache(&app).get(&id).unwrap().text.len(),
            first_len,
            "a 10-char growth on a 10k block must be debounced",
        );

        let big_growth = format!("{small_growth}{}", "z".repeat(DEBOUNCE_CHARS + 10));
        edit_block(&mut app, id, &big_growth);
        app.update();
        assert!(
            cache(&app).get(&id).unwrap().text.len() > first_len,
            "growth past the debounce threshold must land",
        );
    }
}
