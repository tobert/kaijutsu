//! The conversation surface — main-world core.
//!
//! This is the entity-free half of the conversation-surface rewrite
//! (`docs/conversation-surface.md`): block text is formatted into a cache
//! keyed by `BlockId` ([`content`]), given a border and the layout that
//! border implies ([`chrome`]), shaped once into chunked glyph runs
//! ([`chunk`] + [`shape_cache`]), measured back into
//! [`ConversationGeometry`], and finally reduced to a scroll window of
//! document-space runs and chrome quads the render world will extract
//! ([`window`] + [`chrome`]). No Bevy UI node, no per-block RTT, no taffy —
//! a block that scrolls into view costs a binary search and a buffer
//! re-upload, not a spawn.
//!
//! Rich content rides the same spine: [`rich`] turns a detected ABC tune,
//! diff, sparkline or image placeholder into the geometry that draws under a
//! block's glyphs, and an SVG into a cached raster the surface draws as a
//! textured quad. Four pipelines, one pass, in that order — chrome, geometry,
//! rasters, glyphs (`text::msdf::surface_renderer`).
//!
//! This is the only conversation renderer — the legacy Bevy-UI column of
//! per-block RTT textures (and the `ConversationRenderPath` flag that used to
//! pick between them) was deleted in slice 5 (Amy, 2026-08-18: "no reason to
//! keep legacy in this project"). The shared spine — `sync_conversation_geometry`,
//! `smooth_scroll`, `scroll_render_mode` — lives in `cell::plugin`/`view::scroll`
//! and is not touched here.
//!
//! # Schedule placement
//!
//! `cell::plugin::CellPhase` runs `Input → Sync → Spawn → Buffer → Layout` in
//! `Update`. `sync_conversation_geometry` is the *last* thing in `Spawn`, and
//! `smooth_scroll` sits in `Layout`. That fixes where the four surface sets
//! belong:
//!
//! - All four sets run **after `sync_conversation_geometry`** (they read row
//!   structure and write measured heights into it) and **after
//!   `smooth_scroll`**, chained Content → Shape → Measure → Window. The
//!   load-bearing part is Shape-after-ease: the shape band must be a
//!   function of the frame's FINAL offset, or a large ease step (G-jump,
//!   flick) moves the window past the rows Shape prepared and the frame
//!   draws a blank band (found in review, 2026-08-18). Anchor compensation
//!   in Measure stays same-frame: it shifts `offset` and `target_offset` by
//!   the same delta after the ease, preserving the eased trajectory, and
//!   extraction (ExtractSchedule, after all of Update) reads the corrected
//!   value. The ease's `max_offset` clamp sees `content_height` one frame
//!   stale — the same phase relationship the legacy path's own `PostUpdate`
//!   height readback once had.
//! - [`SurfaceSet::Window`] last, because the window is a function of the
//!   final offset *and* the heights Measure just applied.
//!
//! Note these sets deliberately do **not** join `CellPhase`: the surface
//! path replaced `Buffer`/`Layout`'s legacy occupants rather than extending
//! them, and `CellPhase` itself now carries no conversation-content systems
//! at all past `Spawn`. The orderings above are expressed against the two
//! systems this module anchors to (`sync_conversation_geometry`,
//! `smooth_scroll`), not against a phase.
//!
//! # Dead code
//!
//! This module carried a blanket `#![allow(dead_code)]` through slices 1-2,
//! covering groundwork later slices would consume. Slice 3 consumed it —
//! `ShapedChunk::byte_range` (the incremental tail), `ShapedBlock::glyph_count`
//! and `ShapedBlockCache::total_glyphs` (the eviction budget),
//! `ShapedBlock::last_used` (the LRU order) — so the blanket allow is gone.
//! What is left is allowed **item by item**, each with its reason, which is
//! the point: a new unused item now shows up as a warning instead of hiding
//! under a module-wide waiver.

use std::ops::Range;

use bevy::prelude::*;
use bevy::render::{
    ExtractSchedule, Render, RenderApp, RenderSystems, render_resource::PipelineCache,
    renderer::RenderDevice,
};

pub mod chrome;
pub mod chunk;
pub mod content;
pub mod extract;
pub mod labels;
pub mod rich;
pub mod shape_cache;
pub mod target;
pub mod window;

/// Everything a built window depends on *besides* the scroll offset.
///
/// The offset is handled separately (see [`SurfaceWindow::built_scroll_band`])
/// because it moves continuously and must not invalidate the window on every
/// pixel; each field here is a discrete token that, when it moves, means the
/// previously assembled runs are wrong rather than merely shifted.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Reflect)]
pub struct WindowKey {
    /// `ConversationGeometry::epoch()` — row structure or offsets moved.
    pub geometry_epoch: u64,
    /// `MsdfAtlas::version` — glyph regions were added or repacked, so every
    /// UV the window resolved is stale even though no glyph moved.
    pub atlas_version: u64,
    /// [`shape_cache::SurfaceMetricsEpoch`] — font size / line height /
    /// label sizing changed, so shaped runs and header labels are stale.
    pub metrics_epoch: u64,
    /// [`shape_cache::SurfaceThemeEpoch`] — colors changed. No glyph moved,
    /// but the runs the window is holding are `Arc`s the recolor pass has
    /// since replaced (`Arc::make_mut` hands the cache a *new* pointer while
    /// an assembled window keeps the old one), and the role labels were
    /// re-shaped outright. So the window must re-assemble even though nothing
    /// re-shaped.
    pub theme_epoch: u64,
    /// [`shape_cache::ShapedBlockCache::generation`] — the cache itself
    /// mutated: a shape landed (sync or async), a block was evicted, a
    /// status-driven recolor ran, a placement refreshed. The theme epoch
    /// cannot stand in for this — `block_color` also encodes *status* (a
    /// tool call finishing goes amber→fg with no theme write), and an async
    /// landing whose height exactly matched its estimate moves neither the
    /// geometry epoch nor anything else here (found by review, 2026-08-18).
    pub shaped_generation: u64,
    /// Pane content-box size in logical px, rounded to whole pixels. Sub-pixel
    /// jitter from layout is not a reason to rebuild.
    pub viewport: (u32, u32),
}

/// The slack window a surface currently holds buffers for.
#[derive(Debug, Default, Clone, Reflect)]
pub struct SurfaceWindow {
    /// Geometry row indices covered by the built window.
    pub row_range: Range<usize>,
    /// Inclusive `[min, max]` scroll offsets this window stays valid for.
    /// Scrolling inside it uploads nothing — the vertex shader's offset
    /// uniform does all the work. See [`window::scroll_band`].
    pub built_scroll_band: (f32, f32),
    /// The discrete state the window was built against.
    pub built_for: WindowKey,
}

/// One conversation pane's drawing surface.
///
/// Spawned one-per-[`crate::cell::ConversationContainer`] by
/// [`window::ensure_conversation_surfaces`]. The RTT + `ImageNode` composite
/// arrives with [`target::attach_surface_targets`]; the chrome instances ride
/// along as a required component so a surface can never exist without a place
/// to put its borders.
#[derive(Component, Debug, Reflect)]
#[reflect(Component)]
#[require(chrome::ChromeInstances)]
pub struct ConversationSurface {
    /// The `ConversationContainer` entity this surface draws for. Its
    /// `ComputedNode` is the viewport size; its pane owns the scroll state.
    pub pane: Entity,
    pub window: SurfaceWindow,
    /// Bumped whenever the assembled runs changed. The extraction step
    /// compares this instead of re-walking the caches.
    pub buffer_version: u64,
}

impl ConversationSurface {
    pub fn new(pane: Entity) -> Self {
        Self {
            pane,
            window: SurfaceWindow::default(),
            buffer_version: 0,
        }
    }
}

/// Execution phases for the surface path. See the module docs for why each
/// one sits where it does relative to `smooth_scroll`.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub enum SurfaceSet {
    /// Format block text into [`content::BlockContentCache`] and its border
    /// into [`chrome::BlockChromeCache`]; keep the per-pane surface entities
    /// in step with the panes.
    Content,
    /// Shape in-window blocks into [`shape_cache::ShapedBlockCache`].
    Shape,
    /// Feed shaped heights back into `ConversationGeometry` and
    /// anchor-compensate the scroll offset.
    Measure,
    /// Decide the slack window each surface holds buffers for, and assemble
    /// the chrome quads that cover it.
    Window,
}

/// Registers the surface path's resources and systems — main world and
/// render world both.
///
/// Registered from `main.rs` **after `CellPlugin`** — the `Update` ordering
/// below anchors to `sync_conversation_geometry` / `smooth_scroll`, which
/// live there.
///
/// The render-world half needs `BlockRenderPlugin`'s resources —
/// `GpuTextureLimits` for texture sizing, `ExtractedMsdfAtlas` and
/// `ExtractedMsdfRenderParams` for the draw — and shares its atlas rather
/// than owning one.
pub struct ConversationSurfacePlugin;

impl Plugin for ConversationSurfacePlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<ConversationSurface>();

        app.init_resource::<content::BlockContentCache>()
            .init_resource::<chrome::BlockChromeCache>()
            .init_resource::<shape_cache::ShapedBlockCache>()
            .init_resource::<shape_cache::HeaderLabelCache>()
            .init_resource::<labels::LabelRunCache>()
            .init_resource::<shape_cache::SurfaceMetricsEpoch>()
            .init_resource::<shape_cache::SurfaceThemeEpoch>()
            .init_resource::<shape_cache::ShapeTasks>()
            .init_resource::<rich::SvgRasterCache>();

        // Content → Shape → Measure → Window, all AFTER the shared scroll
        // ease. Shape's band must be a function of the frame's FINAL offset:
        // with Shape before the ease, a large ease step (G-jump, flick) moved
        // the window past the band Shape had prepared and `assemble_runs`
        // came up empty — a one-frame blank band (gemini deliberation,
        // 2026-08-18, merge blocker). Running Measure after the ease is
        // order-equivalent for anchoring: compensation shifts `offset` and
        // `target_offset` by the same delta, so the eased trajectory is
        // preserved and extraction still reads the corrected offset this
        // frame. The ease's `max_offset` clamp sees `content_height` one
        // frame stale — exactly the phase relationship Legacy has with its
        // PostUpdate `readback_block_heights`.
        app.configure_sets(
            Update,
            (
                SurfaceSet::Content,
                SurfaceSet::Shape,
                SurfaceSet::Measure,
                SurfaceSet::Window,
            )
                .chain()
                .after(crate::view::geometry::sync_conversation_geometry)
                .after(crate::view::scroll::smooth_scroll),
        );

        app.add_systems(
            Update,
            (
                window::ensure_conversation_surfaces,
                target::attach_surface_targets.after(window::ensure_conversation_surfaces),
                shape_cache::track_surface_metrics_epoch,
                shape_cache::track_surface_theme_epoch,
                // Markdown span brushes are built from theme colors, so the
                // theme epoch has to be current before content re-derives
                // them — otherwise a theme swap reuses last frame's palette.
                content::sync_block_content.after(shape_cache::track_surface_theme_epoch),
                // Border styles decide the wrap width, so they must be
                // current before anything shapes against them.
                chrome::sync_block_chrome.after(content::sync_block_content),
            )
                .in_set(SurfaceSet::Content),
        );
        // Backlog results land BEFORE the shape pass looks at the band: a
        // block whose task finished this frame is already in the cache when
        // the pass reaches it, so the pass sees a matching key instead of
        // shaping the same text a second time on the main thread.
        app.add_systems(
            Update,
            (
                shape_cache::apply_shape_results,
                shape_cache::shape_visible_blocks,
                // After shaping, because it re-derives the same wrap widths
                // and must read the frame's, not the previous frame's. It
                // owes the row no height — the shaper computes an SVG's draw
                // size without needing its pixels — so nothing downstream
                // waits on the raster.
                rich::sync_svg_rasters,
            )
                .chain()
                .in_set(SurfaceSet::Shape),
        );
        app.add_systems(
            Update,
            shape_cache::apply_shaped_measurements.in_set(SurfaceSet::Measure),
        );
        app.add_systems(
            Update,
            (
                window::build_surface_window,
                // Chrome rects are a function of the built window's rows, so
                // this follows — and carries its own version, so a focus move
                // or a status pulse never re-uploads a glyph.
                chrome::build_chrome_instances.after(window::build_surface_window),
            )
                .in_set(SurfaceSet::Window),
        );

        // The RTT is sized from live layout every frame (the dock pattern),
        // so it has to run after taffy has produced a `ComputedNode` for the
        // composite node this system's own `attach_surface_targets` spawned.
        app.add_systems(
            PostUpdate,
            target::resize_surface_targets.after(bevy::ui::UiSystems::Layout),
        );

        // ── Render world ────────────────────────────────────────────────
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        render_app
            .init_resource::<extract::ExtractedConversationSurfaces>()
            .init_resource::<extract::SurfaceGpuBuffers>()
            .add_systems(ExtractSchedule, extract::extract_conversation_surfaces)
            .add_systems(
                Render,
                extract::render_conversation_surfaces
                    .in_set(RenderSystems::Render)
                    .run_if(|surfaces: Res<extract::ExtractedConversationSurfaces>| {
                        !surfaces.items.is_empty()
                    }),
            );
    }

    /// The pipeline needs `RenderDevice` + `PipelineCache`, which only exist
    /// after renderer initialization — the same reason
    /// `BlockRenderPlugin::finish` builds `MsdfBlockRenderer` here.
    fn finish(&self, app: &mut App) {
        let Some(render_app) = app.get_sub_app_mut(RenderApp) else {
            return;
        };
        let renderer = {
            let world = render_app.world();
            crate::text::msdf::surface_renderer::ConversationSurfaceRenderer::init(
                world.resource::<RenderDevice>(),
                world.resource::<PipelineCache>(),
                world.resource::<AssetServer>(),
            )
        };
        render_app.insert_resource(renderer);
        info!("Initialized conversation surface renderer in render world");
    }
}
