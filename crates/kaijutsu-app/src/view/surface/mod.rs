//! The conversation surface — main-world core.
//!
//! This is the entity-free half of the conversation-surface rewrite
//! (`docs/conversation-surface.md`): block text is formatted into a cache
//! keyed by `BlockId` ([`content`]), shaped once into chunked glyph runs
//! ([`chunk`] + [`shape_cache`]), measured back into
//! [`ConversationGeometry`], and finally reduced to a scroll window of
//! document-space runs the render world will extract ([`window`]). No Bevy
//! UI node, no per-block RTT, no taffy — a block that scrolls into view
//! costs a binary search and a buffer re-upload, not a spawn.
//!
//! Everything here is **behind the flag**: [`ConversationRenderPath`]
//! selects `Legacy` (today's `cell/plugin.rs` systems) or `Surface` (these),
//! and every system in this plugin carries `run_if(surface_active)`. The
//! shared spine — `sync_conversation_geometry`, `smooth_scroll`,
//! `scroll_render_mode` — runs on both paths and is not touched here.
//!
//! # Schedule placement
//!
//! The legacy conversation runs as five phases in `Update`
//! (`cell::plugin::CellPhase`): `Input → Sync → Spawn → Buffer → Layout`.
//! `sync_conversation_geometry` is the *first* thing in `Spawn`, and
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
//!   stale — the same phase relationship Legacy has with its PostUpdate
//!   `readback_block_heights`.
//! - [`SurfaceSet::Window`] last, because the window is a function of the
//!   final offset *and* the heights Measure just applied.
//!
//! Note these sets deliberately do **not** join `CellPhase`: the surface
//! path replaces `Buffer`/`Layout` rather than extending them, and once the
//! legacy systems are flag-gated off there is no phase left to sit in. The
//! orderings above are expressed against the two systems that survive the
//! rewrite, not against the phases that don't.

// The plugin is registered now, so every *system* here has a caller — what
// this allow still covers is the groundwork later slices consume:
// `ShapedChunk::byte_range` and `ShapedBlock::glyph_count` (slice 3's
// incremental tail + eviction budget), the cache's `total_glyphs`/`tick`,
// `ShapedLabel`'s measured size, and the instance buffers' `len`/`capacity`
// accessors (used from tests and diagnostics). It must come off — or narrow
// to those items — as those slices land, or it will hide genuinely dead code
// in a module built to be pruned.
#![allow(dead_code)]

use std::ops::Range;

use bevy::prelude::*;
use bevy::render::{
    ExtractSchedule, Render, RenderApp, RenderSystems, render_resource::PipelineCache,
    renderer::RenderDevice,
};

pub mod chunk;
pub mod content;
pub mod extract;
pub mod shape_cache;
pub mod target;
pub mod window;

/// Environment variable that selects the render path at startup.
pub const SURFACE_ENV: &str = "KAIJUTSU_CONV_SURFACE";

/// Which conversation renderer is live.
///
/// `Legacy` is the Bevy-UI column of per-block RTT textures; `Surface` is
/// this module's direct-draw path. Reflected so a live session can be A/B'd
/// over BRP (`world_mutate_resources`) without a restart — the whole point
/// of carrying two paths at once.
#[derive(Resource, Reflect, Debug, Default, Clone, Copy, PartialEq, Eq)]
#[reflect(Resource)]
pub enum ConversationRenderPath {
    #[default]
    Legacy,
    Surface,
}

impl ConversationRenderPath {
    /// Read the startup default from [`SURFACE_ENV`]: `1`/`true` (any case)
    /// selects `Surface`, anything else — including an unset or unparseable
    /// value — leaves `Legacy`. Deliberately not an error: this is a
    /// developer A/B switch, and the safe interpretation of a typo is "the
    /// path that has shipped".
    pub fn from_env() -> Self {
        match std::env::var(SURFACE_ENV) {
            Ok(v) => {
                let v = v.trim().to_ascii_lowercase();
                if v == "1" || v == "true" {
                    Self::Surface
                } else {
                    Self::Legacy
                }
            }
            Err(_) => Self::Legacy,
        }
    }
}

/// Run condition: the surface path is live.
pub fn surface_active(path: Res<ConversationRenderPath>) -> bool {
    *path == ConversationRenderPath::Surface
}

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
/// [`window::ensure_conversation_surfaces`]. Carries no UI node of its own in
/// this slice — the RTT + `ImageNode` composition lands with the render-world
/// half.
#[derive(Component, Debug, Reflect)]
#[reflect(Component)]
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
    /// Format block text into [`content::BlockContentCache`]; keep the
    /// per-pane surface entities in step with the panes.
    Content,
    /// Shape in-window blocks into [`shape_cache::ShapedBlockCache`].
    Shape,
    /// Feed shaped heights back into `ConversationGeometry` and
    /// anchor-compensate the scroll offset.
    Measure,
    /// Decide the slack window each surface holds buffers for.
    Window,
}

/// Registers the surface path's resources and systems — main world and
/// render world both.
///
/// Registered from `main.rs` **after `CellPlugin`** (the `Update` ordering
/// anchors `sync_conversation_geometry` / `smooth_scroll` live there) and
/// alongside the flag-gating of the legacy systems: every system here carries
/// `run_if(surface_active)` and its legacy counterpart carries the opposite,
/// so the two paths never both measure.
///
/// The render-world half needs `BlockRenderPlugin`'s resources —
/// `GpuTextureLimits` for texture sizing, `ExtractedMsdfAtlas` and
/// `ExtractedMsdfRenderParams` for the draw — and shares its atlas rather
/// than owning one.
pub struct ConversationSurfacePlugin;

impl Plugin for ConversationSurfacePlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<ConversationRenderPath>()
            .register_type::<ConversationSurface>();

        app.insert_resource(ConversationRenderPath::from_env())
            .init_resource::<content::BlockContentCache>()
            .init_resource::<shape_cache::ShapedBlockCache>()
            .init_resource::<shape_cache::HeaderLabelCache>()
            .init_resource::<shape_cache::SurfaceMetricsEpoch>();

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
                content::sync_block_content,
            )
                .in_set(SurfaceSet::Content)
                .run_if(surface_active),
        );
        app.add_systems(
            Update,
            shape_cache::shape_visible_blocks
                .in_set(SurfaceSet::Shape)
                .run_if(surface_active),
        );
        app.add_systems(
            Update,
            shape_cache::apply_shaped_measurements
                .in_set(SurfaceSet::Measure)
                .run_if(surface_active),
        );
        app.add_systems(
            Update,
            window::build_surface_window
                .in_set(SurfaceSet::Window)
                .run_if(surface_active),
        );

        // The RTT is sized from live layout every frame (the dock pattern),
        // so it has to run after taffy has produced a `ComputedNode` for the
        // composite node this system's own `attach_surface_targets` spawned.
        app.add_systems(
            PostUpdate,
            target::resize_surface_targets
                .after(bevy::ui::UiSystems::Layout)
                .run_if(surface_active),
        );

        // ── Render world ────────────────────────────────────────────────
        //
        // Extraction is gated on the flag inside the system rather than by a
        // `run_if`: the render world has no `ConversationRenderPath` of its
        // own, and mirroring one there would be a second source of truth for
        // a switch that already travels with the extracted data.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_path_defaults_to_legacy() {
        assert_eq!(ConversationRenderPath::default(), ConversationRenderPath::Legacy);
    }

    /// The flag is a run condition, so its two states must actually be
    /// distinguishable through `Res` — a trivial assertion that fails loudly
    /// if the enum ever grows a third state the condition forgets.
    #[test]
    fn surface_active_only_for_the_surface_path() {
        let mut world = World::new();
        world.insert_resource(ConversationRenderPath::Legacy);
        let mut sys = IntoSystem::into_system(surface_active);
        sys.initialize(&mut world);
        assert!(!sys.run((), &mut world).unwrap());

        world.insert_resource(ConversationRenderPath::Surface);
        assert!(sys.run((), &mut world).unwrap());
    }
}
