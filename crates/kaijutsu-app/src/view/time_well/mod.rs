//! Time-well context browser — the first concrete consumer of `kaijutsu-viz`.
//!
//! A radial 3D "well" of context cards, seated by **explicit ring placement**
//! (`docs/timewell.md`, "Ring membership becomes explicit") — two hand-curated
//! rings sandwiching two automatic ones: `Active` (mouth, `p`romoted) →
//! `Recent` (automatic, most-recently-active) → `Bumped` (automatic overflow;
//! concluded contexts compete only here) → `Demoted` (throat, `d`emoted).
//! Every ring seats exactly 10 (`kaijutsu_viz::layout::RING_SLOTS`); anything
//! past seat 9 is the event horizon — no card entity, a "+N" count at the
//! throat. Each ring has its own radius/depth terrace with a visible step +
//! gap at the boundary; within a terrace, angle encodes the ring's own seat
//! order. `docs/time-well-concepts.md` holds the earlier UX record; the
//! retired substrate design survives as the "Appendix: substrate notes" in
//! `docs/timewell.md` (full viz-substrate.md in git history).
//!
//! Module map:
//! - [`card`] — pure `ContextInfo` → card-model mapping, band assignment, layout,
//!   and the `LayoutPos → Vec3` well-lift (no Bevy, unit-tested).
//! - [`scene`] — the 3D scene: camera, root, screen toggle, billboarding, card
//!   motion, components, and live state resource.
//! - [`drape`] — on-selection lineage drapes: curved ribbons down the bowl
//!   wall from the selected card to each fork-ancestor's card (pure curve
//!   math, unit-tested; reuses `TraceGlowMaterial`).
//! - [`sync`] — the layout tick: keyed-join reconcile → spawn/despawn → layout.
//! - [`activity`] — the well's pulse: kernel-event stream → ring energy + ripples
//!   driving the base ring deck (unit-tested math).
//! - [`live`] — live state beyond the poll: per-context tail buffers (the
//!   card face's tail band) and per-track beat phasors (chatter + beat card
//!   lanes, the deck heartbeat).
//! - [`rays`] — tracks as beams down the funnel wall: the `listTracks` poll,
//!   the ray entities at name-stable bearings, and the per-frame beat pulse.
//! - [`legend`] — the transient `?` keyboard legend, the retired edge HUD's
//!   last survivor.

pub mod activity;
pub mod card;
pub mod drape;
pub mod legend;
pub mod live;
pub mod panel;
pub mod rays;
pub mod scene;
pub mod sync;
pub mod text;

use bevy::prelude::*;

use crate::ui::screen::Screen;

/// Wires the time-well browser into the app.
///
/// Slice C (`lovely-swimming-prism.md`, time-well/room integration) split the
/// old single `run_if(in_state(Screen::TimeWell))` tuple into three tiers,
/// mirroring `patch_bay`'s own ambient/`PatchBayLod` split:
/// - **fully ungated** — runs on every screen, `live::ingest_live_events`'s
///   existing pattern ("the well opens warm"). [`scene::tick_ring_activity`]
///   joins it this slice: the `RingActivity` decay tick must not freeze
///   outside the room (see its own doc for the bug that guards against).
/// - **ambient** (`run_if(in_state(Screen::Room))`) — the well IS room
///   furniture now; everything that keeps its cards/rings/rays live and
///   correct at room scale runs here, cards included (the "opens warm"
///   property extends to the well's own contents, not just tails/beats).
///   The dim/selection/lineage/drift overlays and the focus-card/horizon-
///   label LOD gates joined this tier in the freeze-fix slice (2026-07-11):
///   each must react to BOTH zoom directions — clearing/hiding on zoom-OUT,
///   not just applying on zoom-in — the same reasoning
///   `legend::despawn_legend_unzoomed` needs to justify running here rather
///   than dived-only. Left dived-only, they froze whatever dim/pop/lineage
///   state was live on the last dived frame, visible at room scale until the
///   next dive re-ran them.
/// - **dived-only** (`run_if(well_zoomed)`) — keyboard, the legend toggle, and
///   the card/reading/horizon TEXT builders: room-scale card text is
///   unreadable pixels, so text rasterization stays gated to the dive
///   (`text::build_card_scenes`'s own doc has the reasoning + the
///   `card_text_dirty` catch-up arm).
pub struct TimeWellPlugin;

impl Plugin for TimeWellPlugin {
    fn build(&self, app: &mut App) {
        app.add_plugins(MaterialPlugin::<crate::shaders::WellCardMaterial>::default())
            .add_plugins(MaterialPlugin::<crate::shaders::WellRingsMaterial>::default())
            .add_plugins(MaterialPlugin::<crate::shaders::TerraceRingMaterial>::default())
            .add_plugins(MaterialPlugin::<crate::shaders::TrackRayMaterial>::default())
            .init_resource::<scene::TimeWellState>()
            .init_resource::<activity::RingActivity>()
            .init_resource::<live::ContextTails>()
            .init_resource::<live::WellBeats>()
            .init_resource::<rays::WellTracks>()
            // Go-to-well runs on every screen (prefix/gamepad reach it from
            // anywhere); it consumes ActionFired, so it follows dispatch.
            .add_systems(
                Update,
                scene::handle_go_to_well.after(crate::input::InputPhase::Dispatch),
            )
            // Fully ungated: opens warm on every screen, well included.
            .add_systems(Update, (live::ingest_live_events, scene::tick_ring_activity))
            // Ambient: room-scale truth. The well breathes here whether
            // you're dived into it or just walking past its bearing.
            .add_systems(
                Update,
                // Two nested `.chain()`ed groups, not one flat tuple: Bevy's
                // `IntoSystemConfigs` tuple impl tops out below the ~21
                // systems this tier now runs (the drape overlay tipped it
                // over) — nesting preserves full mouth-to-throat ordering
                // (every system in group A still runs before every system in
                // group B) while staying under that ceiling. Split at the
                // same "pulse/placement plumbing" vs "selection-driven
                // overlays" seam the comments below already draw.
                (
                    (
                        sync::poll_clusters,
                        sync::apply_clusters,
                        rays::poll_tracks,
                        rays::apply_tracks,
                        rays::sync_track_rays,
                        rays::animate_track_rays,
                        sync::sync_time_well,
                        scene::spin_rings,
                        scene::move_cards_toward_target,
                        scene::billboard_cards,
                        // Ambient, not ungated (unlike its sibling
                        // `tick_ring_activity`): needs live `Card`/`CardTarget`
                        // entities to resolve an event's ring angle, and those
                        // only exist while the well's furniture is spawned
                        // (Screen::Room).
                        scene::accumulate_ring_activity,
                        scene::sync_deck_material,
                        live::sync_card_live_uniforms,
                        // Moved here from dived-only (freeze-fix slice,
                        // 2026-07-11): each must react to BOTH zoom directions —
                        // see this file's own doc comment above for the full
                        // reasoning, and each system's own doc for its specific
                        // zoom branch. `highlight_drift` is the one exception: no
                        // zoom branch at all (see its doc) — `DriftState` polls
                        // ungated on every screen, so its shimmer is truthful
                        // live info even at room scale.
                        scene::dim_nonfocused_rings,
                        scene::sync_focus_card_visibility,
                        scene::apply_horizon_label_lod,
                    )
                        .chain(),
                    (
                        scene::highlight_selection,
                        scene::highlight_lineage,
                        // The spatial lineage overlay (draped ribbons down
                        // the bowl wall) — right after the ring-highlight it
                        // was derived from, so it reads the same frame's
                        // card positions `move_cards_toward_target`/
                        // `spin_rings` (group A, above) just settled.
                        drape::sync_lineage_drapes,
                        scene::highlight_drift,
                        // The transient legend's dismissal lives in the ambient
                        // tier, not dived-only, like `patch_bay::apply_patch_lod`
                        // — it must react to BOTH transitions (dismissing the
                        // legend on zoom-OUT, not just leaving it be on zoom-in),
                        // so it has to keep running at room scale even while
                        // unzoomed.
                        legend::despawn_legend_unzoomed,
                    )
                        .chain(),
                )
                    .chain()
                    .run_if(in_state(Screen::Room)),
            )
            // Dived-only: keyboard, the legend toggle, and the text builders
            // (rasterizing MSDF glyphs no one can read at room scale would be
            // pure waste — see `text::build_card_scenes`).
            .add_systems(
                Update,
                (
                    // Ordering vs `room_keyboard` is no longer load-bearing —
                    // ActionFired carries the binding context that matched, so
                    // a WellZoomed Esc can never replay through the room's
                    // consumer (the 2026-07-11 same-frame double-fire is
                    // structurally gone). Kept `.after(InputPhase::Dispatch)`
                    // via the group below so actions land same-frame.
                    scene::well_keyboard,
                    legend::toggle_legend,
                    legend::position_legend,
                    // Writes `Card::tail` (guarded — only on real change)
                    // right before the text builder that reads it, so a
                    // fresh tail lands in the SAME frame's rebuild rather
                    // than waiting a tick.
                    live::sync_selected_card_tail,
                    text::build_card_scenes,
                    // Shader-param-only half of the old combined material
                    // sync (see its own doc) — decoupled from the glyph
                    // rebuild above so a selection/lineage/drift/status flip
                    // doesn't force one.
                    text::sync_card_material_params,
                    text::update_reading_card,
                    text::build_horizon_label,
                )
                    .chain()
                    .after(crate::input::InputPhase::Dispatch)
                    .run_if(|room: Res<crate::view::room::RoomState>| scene::well_zoomed(&room)),
            );
    }
}
