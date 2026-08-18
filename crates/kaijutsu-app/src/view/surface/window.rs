//! The scroll window: which rows a surface currently holds buffers for, and
//! the document-space runs assembled from them.
//!
//! This is the module that makes the invariant true — *scrolling changes one
//! number and touches nothing else*. The window covers the viewport plus a
//! screen of slack on each side; inside it, a scroll is a uniform update and
//! nothing here runs at all. Only when the offset leaves the band (or one of
//! the discrete [`WindowKey`] tokens moves) does the window rebuild and
//! `buffer_version` tick, which is the extractor's cue to re-upload.

use bevy::prelude::*;

use crate::cell::{ConversationContainer, ConversationScrollState, EditorEntities};
use crate::text::msdf::{MsdfAtlas, PositionedGlyph};
use crate::view::geometry::{ConversationGeometry, RowKey};

use super::shape_cache::{HeaderLabelCache, ShapedBlockCache, SurfaceMetricsEpoch, SurfaceThemeEpoch};
use super::{ConversationSurface, WindowKey};

/// Screens of slack held on each side of the viewport.
pub const WINDOW_SLACK_SCREENS: f32 = 1.0;

/// Fraction of a screen trimmed off each end of the valid scroll band, so a
/// window rebuild doesn't sit right on the edge of the offset that triggered
/// it (which would rebuild again on the next detent back).
pub const WINDOW_HYSTERESIS_SCREENS: f32 = 0.5;

/// One drawable run of glyphs in **document space**.
///
/// `doc_y` is the run's top in the same coordinate space
/// `ConversationGeometry` and `ConversationScrollState.offset` use; the
/// glyphs inside are chunk-local. The renderer's vertex shader does
/// `(doc_y + glyph.y - offset) * scale`, which is why nothing here ever
/// needs to know the scroll position.
#[derive(Debug, Clone)]
pub struct SurfaceRun {
    pub doc_y: f32,
    /// Horizontal offset of the run's origin — a block's indent, or a role
    /// label's inset.
    pub x_offset: f32,
    pub glyphs: std::sync::Arc<Vec<PositionedGlyph>>,
}

/// The inclusive scroll-offset band a window built at `offset` stays valid
/// for.
///
/// The window covers document `[offset - slack, offset + vh + slack]`. A
/// viewport at `offset'` spans `[offset', offset' + vh]`, which fits inside
/// that iff `offset - slack <= offset' <= offset + slack`. Trimming
/// [`WINDOW_HYSTERESIS_SCREENS`] of a screen off each end leaves margin for
/// the rebuild itself, so scrolling back and forth across the boundary
/// doesn't rebuild on every detent.
///
/// A slack too small to survive the trim collapses to the exact offset rather
/// than inverting — an honest "rebuild on any movement" instead of a band
/// that silently reports every offset as valid.
pub fn scroll_band(offset: f32, viewport_height: f32, slack: f32) -> (f32, f32) {
    let trim = WINDOW_HYSTERESIS_SCREENS * viewport_height;
    let half = slack - trim;
    if half <= 0.0 {
        return (offset, offset);
    }
    (offset - half, offset + half)
}

/// Whether `offset` is inside a built band.
fn band_contains(band: (f32, f32), offset: f32) -> bool {
    offset >= band.0 && offset <= band.1
}

/// Reduce a row range to the glyph runs that draw it.
///
/// Pure — no ECS — so the assembly the extractor performs is testable without
/// a render world. Block rows contribute one run per shaped chunk, stacked by
/// chunk height from the row's `y_offset`; header rows contribute their
/// cached role label, positioned inside the header row.
///
/// Rows with nothing shaped yet are simply absent from the output: on the
/// surface path a not-yet-shaped row draws as empty space of the height the
/// geometry already reserved, which is the same thing an estimate always
/// meant. Slice 3 kept it that way rather than adding a placeholder
/// treatment — the row still gets its chrome and its reserved height, the gap
/// only lasts as long as a backlog task, and a skeleton drawn under text that
/// is about to arrive reads as a flicker rather than as progress.
pub fn assemble_runs(
    geom: &ConversationGeometry,
    shaped: &ShapedBlockCache,
    headers: &HeaderLabelCache,
    metrics_epoch: u64,
    row_range: std::ops::Range<usize>,
) -> Vec<SurfaceRun> {
    let rows = geom.rows();
    let start = row_range.start.min(rows.len());
    let end = row_range.end.min(rows.len());

    let mut runs = Vec::new();
    for row in &rows[start..end] {
        match row.key {
            RowKey::Block(id) => {
                let Some(block) = shaped.get(&id) else {
                    continue;
                };
                // `y_offset` is the border's top padding: the text starts
                // inside the box chrome draws, not at the row's top edge.
                let mut y = row.y_offset + block.y_offset;
                for chunk in &block.chunks {
                    if !chunk.glyphs.is_empty() {
                        runs.push(SurfaceRun {
                            doc_y: y,
                            x_offset: block.x_offset,
                            glyphs: chunk.glyphs.clone(),
                        });
                    }
                    y += chunk.height;
                }
            }
            RowKey::Header(_) => {
                let Some(label) = headers.get(row.role, metrics_epoch) else {
                    continue;
                };
                runs.push(SurfaceRun {
                    doc_y: row.y_offset + label.y_offset,
                    x_offset: label.x_offset,
                    glyphs: label.glyphs.clone(),
                });
            }
        }
    }
    runs
}

/// Keep one [`ConversationSurface`] per [`ConversationContainer`].
///
/// No UI node is touched here — the surface is a bare entity holding the
/// window state. The RTT + `ImageNode` composition (and the pane-split
/// wiring that `ui/tiling_reconciler.rs` owns) lands with the render-world
/// half; this system exists so that work has something to attach to.
pub fn ensure_conversation_surfaces(
    mut commands: Commands,
    containers: Query<Entity, With<ConversationContainer>>,
    surfaces: Query<(Entity, &ConversationSurface)>,
) {
    let live: std::collections::HashSet<Entity> = containers.iter().collect();
    let mut covered: std::collections::HashSet<Entity> = std::collections::HashSet::new();

    for (entity, surface) in &surfaces {
        // A surface whose pane is gone — or a duplicate for a pane already
        // covered — is despawned rather than left to draw at a stale size.
        if live.contains(&surface.pane) && covered.insert(surface.pane) {
            continue;
        }
        commands.entity(entity).try_despawn();
    }

    for pane in live {
        if !covered.contains(&pane) {
            commands.spawn(ConversationSurface::new(pane));
        }
    }
}

/// Decide the slack window each surface holds buffers for.
///
/// Runs **after `smooth_scroll`** so the offset it bands around is the final
/// one for the frame. Two ways to do nothing, and the common case is both:
/// the offset is still inside the built band, and every discrete input is
/// unchanged.
pub fn build_surface_window(
    entities: Res<EditorEntities>,
    geometries: Query<&ConversationGeometry>,
    computed_nodes: Query<&ComputedNode>,
    scroll_state: Res<ConversationScrollState>,
    atlas: Option<Res<MsdfAtlas>>,
    metrics_epoch: Res<SurfaceMetricsEpoch>,
    theme_epoch: Res<SurfaceThemeEpoch>,
    shaped: Res<ShapedBlockCache>,
    mut surfaces: Query<&mut ConversationSurface>,
) {
    let Some(main_ent) = entities.main_cell else {
        return;
    };
    let Ok(geom) = geometries.get(main_ent) else {
        return;
    };

    // An absent atlas is a render world that hasn't come up; it contributes
    // `0` to the key and the first real version bump rebuilds the window.
    let atlas_version = atlas.map(|a| a.version).unwrap_or(0);
    let metrics_epoch = metrics_epoch.get();
    let theme_epoch = theme_epoch.get();
    let shaped_generation = shaped.generation();
    let offset = scroll_state.offset;

    for mut surface in surfaces.iter_mut() {
        let pane_size = computed_nodes
            .get(surface.pane)
            .ok()
            .map(crate::view::ui_rtt::logical_content_size);
        let Some(pane_size) = pane_size else {
            // The pane hasn't been laid out — there is no viewport to build a
            // window for, and guessing one would size the slack band wrong.
            continue;
        };
        let vh = if pane_size.y > 0.0 {
            pane_size.y
        } else {
            scroll_state.visible_height
        };
        if vh <= 0.0 {
            continue;
        }

        let wanted = WindowKey {
            geometry_epoch: geom.epoch(),
            atlas_version,
            metrics_epoch,
            theme_epoch,
            shaped_generation,
            viewport: (
                pane_size.x.round().max(0.0) as u32,
                pane_size.y.round().max(0.0) as u32,
            ),
        };

        // A built-empty window is as valid as a built-full one: an empty
        // document's `0..0` range must NOT force a rebuild every frame
        // (found by review, 2026-08-18 — the old `!row_range.is_empty()`
        // clause here did exactly that, bumping `buffer_version` and
        // re-uploading empty runs each tick). "Never built" is already
        // unmatchable: the `vh <= 0` guard above means `wanted.viewport` is
        // nonzero, so it can never equal `WindowKey::default()`.
        if surface.window.built_for == wanted
            && band_contains(surface.window.built_scroll_band, offset)
        {
            continue;
        }

        let slack = WINDOW_SLACK_SCREENS * vh;
        let row_range = geom.visible_rows(offset, vh, slack);
        let band = scroll_band(offset, vh, slack);

        let surface = surface.as_mut();
        surface.window.row_range = row_range;
        surface.window.built_scroll_band = band;
        surface.window.built_for = wanted;
        surface.buffer_version = surface.buffer_version.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kaijutsu_types::{BlockId, BlockKind, ContextId, PrincipalId, Role};
    use std::sync::Arc;

    use crate::text::msdf::glyph::{FontId, GlyphKey};
    use crate::view::geometry::{EstimateParams, RowSeed};
    use crate::view::surface::shape_cache::{ShapeKey, ShapedBlock, ShapedChunk, ShapedLabel};

    fn bid(seq: u64) -> BlockId {
        use std::sync::OnceLock;
        static IDS: OnceLock<(ContextId, PrincipalId)> = OnceLock::new();
        let (ctx, prin) = IDS.get_or_init(|| (ContextId::new(), PrincipalId::new()));
        BlockId::new(*ctx, *prin, seq)
    }

    fn params() -> EstimateParams {
        EstimateParams {
            cols: 100,
            line_height: 30.0,
            block_spacing: 12.0,
            role_header_height: 20.0,
            role_header_spacing: 4.0,
        }
    }

    fn seed() -> RowSeed {
        RowSeed {
            text_len: 40,
            newline_count: 0,
            role: Role::User,
            kind: BlockKind::Text,
            collapsed: false,
            parent_id: None,
        }
    }

    /// header (20 + 4) at y=0, then `n` blocks of 42 pitch from y=24.
    fn strip(n: u64) -> ConversationGeometry {
        let mut geom = ConversationGeometry::default();
        let ids: Vec<BlockId> = (1..=n).map(bid).collect();
        geom.reconcile(&ids, |_| Some(seed()), &params(), 1);
        geom.recompute_offsets();
        geom
    }

    fn glyph(x: f32) -> PositionedGlyph {
        PositionedGlyph {
            key: GlyphKey::new(FontId::for_test(1), 7),
            x,
            y: 0.0,
            font_size: 16.0,
            color: [255, 255, 255, 255],
            importance: 0.5,
        }
    }

    fn shaped_block(x_offset: f32, chunk_heights: &[f32]) -> ShapedBlock {
        let chunks: Vec<ShapedChunk> = chunk_heights
            .iter()
            .enumerate()
            .map(|(i, h)| ShapedChunk {
                byte_range: i..i + 1,
                glyphs: Arc::new(vec![glyph(i as f32)]),
                height: *h,
                frozen: false,
            })
            .collect();
        ShapedBlock {
            key: ShapeKey {
                content_version: 1,
                wrap_width_bits: 800.0_f32.to_bits(),
                collapsed: false,
                indent_level: 0,
                metrics_epoch: 1,
            },
            height: chunk_heights.iter().sum(),
            text_height: chunk_heights.iter().sum(),
            chunks,
            x_offset,
            y_offset: 0.0,
            glyph_count: chunk_heights.len(),
            last_used: 0,
            color: Color::WHITE,
            text_len: 0,
            streaming: false,
        }
    }

    // ---- scroll_band -----------------------------------------------------

    /// The hysteresis math, pinned: at one screen of slack the window stays
    /// valid for half a screen of scrolling in either direction. Widening
    /// this past the slack would draw blank bands; narrowing it to zero would
    /// rebuild every frame.
    #[test]
    fn scroll_band_is_slack_minus_half_a_screen_each_side() {
        let (lo, hi) = scroll_band(1000.0, 600.0, 600.0);
        assert_eq!((lo, hi), (700.0, 1300.0));
        assert!(band_contains((lo, hi), 1000.0));
        assert!(band_contains((lo, hi), 700.0));
        assert!(band_contains((lo, hi), 1300.0));
        assert!(!band_contains((lo, hi), 699.0));
        assert!(!band_contains((lo, hi), 1301.0));
    }

    #[test]
    fn a_slack_too_small_for_the_hysteresis_collapses_to_the_offset() {
        let band = scroll_band(500.0, 600.0, 100.0);
        assert_eq!(band, (500.0, 500.0));
        assert!(band_contains(band, 500.0));
        assert!(!band_contains(band, 501.0));
    }

    // ---- assemble_runs ---------------------------------------------------

    #[test]
    fn chunks_stack_from_the_rows_document_offset() {
        let geom = strip(3);
        let mut shaped = ShapedBlockCache::default();
        shaped.insert_for_test(bid(1), shaped_block(0.0, &[10.0, 20.0, 5.0]));
        let headers = HeaderLabelCache::default();

        // Row 1 is block 1, at y=24.
        let runs = assemble_runs(&geom, &shaped, &headers, 0, 1..2);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].doc_y, 24.0);
        assert_eq!(runs[1].doc_y, 34.0);
        assert_eq!(runs[2].doc_y, 54.0);
    }

    /// A bordered block's text starts inside its top padding — the same inset
    /// the chrome quad reserves (`chrome::BlockLayout`). Without it the first
    /// line sits on the border stroke.
    #[test]
    fn the_border_padding_pushes_the_runs_down_inside_the_row() {
        let geom = strip(3);
        let mut shaped = ShapedBlockCache::default();
        let mut block = shaped_block(0.0, &[10.0, 20.0]);
        block.y_offset = 8.0;
        shaped.insert_for_test(bid(1), block);

        let runs = assemble_runs(&geom, &shaped, &HeaderLabelCache::default(), 0, 1..2);
        assert_eq!(runs[0].doc_y, 32.0, "row y (24) + the top padding (8)");
        assert_eq!(runs[1].doc_y, 42.0);
    }

    #[test]
    fn indent_becomes_the_runs_x_offset() {
        let geom = strip(2);
        let mut shaped = ShapedBlockCache::default();
        shaped.insert_for_test(bid(1), shaped_block(48.0, &[10.0]));
        let runs = assemble_runs(&geom, &shaped, &HeaderLabelCache::default(), 0, 1..2);
        assert_eq!(runs[0].x_offset, 48.0);
    }

    #[test]
    fn rows_outside_the_range_are_excluded() {
        let geom = strip(3);
        let mut shaped = ShapedBlockCache::default();
        for seq in 1..=3 {
            shaped.insert_for_test(bid(seq), shaped_block(0.0, &[10.0]));
        }
        // Rows: 0 = header, 1..=3 = blocks 1..=3.
        let runs = assemble_runs(&geom, &shaped, &HeaderLabelCache::default(), 0, 2..3);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].doc_y, geom.block_row(&bid(2)).unwrap().y_offset);
    }

    #[test]
    fn a_range_past_the_end_is_clamped_rather_than_panicking() {
        let geom = strip(2);
        let shaped = ShapedBlockCache::default();
        let runs = assemble_runs(&geom, &shaped, &HeaderLabelCache::default(), 0, 0..999);
        assert!(runs.is_empty());
    }

    #[test]
    fn header_rows_contribute_their_role_label_inside_the_row() {
        let geom = strip(2);
        let shaped = ShapedBlockCache::default();
        let mut headers = HeaderLabelCache::default();
        headers.insert_for_test(
            Role::User,
            3,
            ShapedLabel {
                glyphs: Arc::new(vec![glyph(0.0)]),
                x_offset: 18.0,
                y_offset: 4.0,
                width: 40.0,
                height: 12.0,
            },
        );

        // Row 0 is the header, at y=0.
        let runs = assemble_runs(&geom, &shaped, &headers, 3, 0..1);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].doc_y, 4.0);
        assert_eq!(runs[0].x_offset, 18.0);

        // A different metrics epoch has no label cached — nothing is drawn
        // rather than something stale.
        assert!(assemble_runs(&geom, &shaped, &headers, 4, 0..1).is_empty());
    }

    #[test]
    fn an_unshaped_block_contributes_nothing() {
        let geom = strip(2);
        let runs = assemble_runs(
            &geom,
            &ShapedBlockCache::default(),
            &HeaderLabelCache::default(),
            0,
            0..4,
        );
        assert!(runs.is_empty());
    }

    // ---- build_surface_window --------------------------------------------

    fn window_app(offset: f32) -> App {
        let mut app = App::new();
        app.init_resource::<EditorEntities>();
        app.init_resource::<SurfaceMetricsEpoch>();
        app.init_resource::<SurfaceThemeEpoch>();
        app.init_resource::<ShapedBlockCache>();
        app.insert_resource(ConversationScrollState {
            offset,
            target_offset: offset,
            visible_height: 300.0,
            following: false,
            ..default()
        });
        app.add_systems(Update, build_surface_window);
        app
    }

    /// A pane node laid out at `300` logical px tall at 1x — the
    /// `ComputedNode` `build_surface_window` reads its viewport from.
    fn pane(app: &mut App) -> Entity {
        app.world_mut()
            .spawn(ComputedNode {
                size: Vec2::new(800.0, 300.0),
                inverse_scale_factor: 1.0,
                ..Default::default()
            })
            .id()
    }

    fn setup_window_app(offset: f32) -> (App, Entity) {
        let mut app = window_app(offset);
        let geom_ent = app.world_mut().spawn(strip(40)).id();
        app.world_mut().resource_mut::<EditorEntities>().main_cell = Some(geom_ent);
        let pane_ent = pane(&mut app);
        let surface = app
            .world_mut()
            .spawn(ConversationSurface::new(pane_ent))
            .id();
        app.update();
        (app, surface)
    }

    fn surface_of(app: &App, entity: Entity) -> &ConversationSurface {
        app.world().get::<ConversationSurface>(entity).unwrap()
    }

    #[test]
    fn the_first_pass_builds_a_window_and_bumps_the_buffer_version() {
        let (app, surface) = setup_window_app(400.0);
        let s = surface_of(&app, surface);
        assert_eq!(s.buffer_version, 1);
        assert!(!s.window.row_range.is_empty());
        assert_eq!(s.window.built_scroll_band, scroll_band(400.0, 300.0, 300.0));
    }

    /// The invariant, at the seam that enforces it: scrolling inside the
    /// built band must not rebuild anything.
    #[test]
    fn scrolling_inside_the_band_leaves_the_buffer_version_alone() {
        let (mut app, surface) = setup_window_app(400.0);
        let before = surface_of(&app, surface).buffer_version;
        let range_before = surface_of(&app, surface).window.row_range.clone();

        app.world_mut()
            .resource_mut::<ConversationScrollState>()
            .offset = 500.0; // +100, well inside ±150
        app.update();

        let s = surface_of(&app, surface);
        assert_eq!(s.buffer_version, before, "an in-band scroll must upload nothing");
        assert_eq!(s.window.row_range, range_before);
    }

    #[test]
    fn scrolling_out_of_the_band_rebuilds_the_window() {
        let (mut app, surface) = setup_window_app(400.0);
        let before = surface_of(&app, surface).buffer_version;

        app.world_mut()
            .resource_mut::<ConversationScrollState>()
            .offset = 900.0;
        app.update();

        let s = surface_of(&app, surface);
        assert_eq!(s.buffer_version, before + 1);
        assert!(band_contains(s.window.built_scroll_band, 900.0));
    }

    /// The atlas repacking moves every UV without moving a glyph: no
    /// reshaping, but the window must re-resolve.
    #[test]
    fn an_atlas_version_bump_rebuilds_the_window() {
        let (mut app, surface) = setup_window_app(400.0);
        let before = surface_of(&app, surface).buffer_version;

        let mut images = Assets::<Image>::default();
        let mut atlas = MsdfAtlas::new(&mut images, 64, 64);
        atlas.version = 999;
        app.insert_resource(atlas);
        app.update();

        assert_eq!(surface_of(&app, surface).buffer_version, before + 1);
    }

    /// A cache mutation with nothing else moving must reach the GPU: a
    /// status-driven recolor (tool call finishing amber→fg) or an async
    /// landing whose height exactly matched its estimate bumps NO other
    /// `WindowKey` field, and without `shaped_generation` the window never
    /// re-assembled and the screen kept the stale colors indefinitely
    /// (review find, 2026-08-18).
    #[test]
    fn a_shaped_cache_mutation_alone_rebuilds_the_window() {
        let (mut app, surface) = setup_window_app(400.0);
        let before = surface_of(&app, surface).buffer_version;

        app.world_mut()
            .resource_mut::<ShapedBlockCache>()
            .insert_for_test(bid(1), shaped_block(0.0, &[30.0]));
        app.update();

        assert_eq!(surface_of(&app, surface).buffer_version, before + 1);
    }

    #[test]
    fn a_surface_whose_pane_is_not_laid_out_builds_nothing() {
        let mut app = window_app(0.0);
        let geom_ent = app.world_mut().spawn(strip(10)).id();
        app.world_mut().resource_mut::<EditorEntities>().main_cell = Some(geom_ent);
        // A pane entity with no ComputedNode at all.
        let pane_ent = app.world_mut().spawn_empty().id();
        let surface = app
            .world_mut()
            .spawn(ConversationSurface::new(pane_ent))
            .id();

        app.update();

        assert_eq!(surface_of(&app, surface).buffer_version, 0);
    }

    // ---- ensure_conversation_surfaces ------------------------------------

    fn lifecycle_app() -> App {
        let mut app = App::new();
        app.add_systems(Update, ensure_conversation_surfaces);
        app
    }

    fn surface_panes(app: &mut App) -> Vec<Entity> {
        let mut query = app.world_mut().query::<&ConversationSurface>();
        query.iter(app.world()).map(|s| s.pane).collect()
    }

    #[test]
    fn one_surface_is_spawned_per_conversation_container() {
        let mut app = lifecycle_app();
        let a = app.world_mut().spawn(ConversationContainer).id();
        let b = app.world_mut().spawn(ConversationContainer).id();
        app.update();

        let mut panes = surface_panes(&mut app);
        panes.sort();
        let mut expected = vec![a, b];
        expected.sort();
        assert_eq!(panes, expected);

        // Idempotent: a second pass spawns nothing new.
        app.update();
        assert_eq!(surface_panes(&mut app).len(), 2);
    }

    #[test]
    fn a_surface_whose_pane_is_gone_is_despawned() {
        let mut app = lifecycle_app();
        let pane = app.world_mut().spawn(ConversationContainer).id();
        app.update();
        assert_eq!(surface_panes(&mut app).len(), 1);

        app.world_mut().entity_mut(pane).despawn();
        app.update();
        assert!(surface_panes(&mut app).is_empty());
    }

    #[test]
    fn a_duplicate_surface_for_one_pane_is_reaped() {
        let mut app = lifecycle_app();
        let pane = app.world_mut().spawn(ConversationContainer).id();
        app.world_mut().spawn(ConversationSurface::new(pane));
        app.world_mut().spawn(ConversationSurface::new(pane));
        app.update();
        assert_eq!(surface_panes(&mut app), vec![pane]);
    }
}
