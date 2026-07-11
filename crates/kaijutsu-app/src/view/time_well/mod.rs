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
//! - [`sync`] — the layout tick: keyed-join reconcile → spawn/despawn → layout.
//! - [`activity`] — the well's pulse: kernel-event stream → ring energy + ripples
//!   driving the base ring deck (unit-tested math).
//! - [`live`] — live state beyond the poll: per-context tail buffers (the
//!   HUD's tail -f view) and per-track beat phasors (chatter + beat card
//!   lanes, the deck heartbeat).
//! - [`rays`] — tracks as beams down the funnel wall: the `listTracks` poll,
//!   the ray entities at name-stable bearings, and the per-frame beat pulse.

pub mod activity;
pub mod card;
pub mod hud;
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
///   `hud::apply_well_hud_lod` already needed to justify running here
///   rather than dived-only. Left dived-only, they froze whatever
///   dim/pop/lineage state was live on the last dived frame, visible at
///   room scale until the next dive re-ran them.
/// - **dived-only** (`run_if(well_zoomed)`) — keyboard and the card/reading/
///   horizon TEXT builders: room-scale card text is unreadable pixels, so
///   text rasterization stays gated to the dive (`text::build_card_scenes`'s
///   own doc has the reasoning + the `card_text_dirty` catch-up arm). The
///   HUD's own per-frame content builders (`hud::position_well_hud`,
///   `hud::update_well_hud`) stay dived-only too — only its LOD gate
///   (`hud::apply_well_hud_lod`) needs the ambient tier.
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
            // The toggle runs in every screen (it decides based on current state).
            .add_systems(Update, scene::toggle_time_well)
            // Fully ungated: opens warm on every screen, well included.
            .add_systems(Update, (live::ingest_live_events, scene::tick_ring_activity))
            // Ambient: room-scale truth. The well breathes here whether
            // you're dived into it or just walking past its bearing.
            .add_systems(
                Update,
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
                    scene::highlight_selection,
                    scene::highlight_lineage,
                    scene::highlight_drift,
                    // The HUD's LOD gate lives in the ambient tier, not
                    // dived-only, like `patch_bay::apply_patch_lod` — it must
                    // react to BOTH transitions (hiding the panels again on
                    // zoom-OUT, not just showing them on zoom-in), so it has
                    // to keep running at room scale even while unzoomed.
                    hud::apply_well_hud_lod,
                )
                    .chain()
                    .run_if(in_state(Screen::Room)),
            )
            // Dived-only: keyboard, the HUD's per-frame content, and the
            // text builders (rasterizing MSDF glyphs no one can read at room
            // scale would be pure waste — see `text::build_card_scenes`).
            .add_systems(
                Update,
                (
                    // `.after(room_keyboard)` (kaibo review, 2026-07-11): must
                    // observe `RoomState::zoomed` from BEFORE this system's own
                    // Esc handler can clear it this frame, or a same-frame
                    // Escape double-fires through both handlers and skips the
                    // room-overview stop — see `room::room_keyboard`'s own doc.
                    scene::well_keyboard.after(crate::view::room::room_keyboard),
                    hud::position_well_hud,
                    hud::update_well_hud,
                    text::build_card_scenes,
                    text::update_reading_card,
                    text::build_horizon_label,
                )
                    .chain()
                    .run_if(|room: Res<crate::view::room::RoomState>| scene::well_zoomed(&room)),
            );
    }
}
