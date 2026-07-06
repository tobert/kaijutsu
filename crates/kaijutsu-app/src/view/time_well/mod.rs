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
            .add_systems(
                OnEnter(Screen::TimeWell),
                (scene::enter_time_well, hud::spawn_well_hud),
            )
            .add_systems(
                OnExit(Screen::TimeWell),
                (scene::exit_time_well, hud::despawn_well_hud, rays::despawn_track_rays),
            )
            // The toggle runs in every screen (it decides based on current state).
            .add_systems(Update, scene::toggle_time_well)
            // Live ingest runs in every screen too, so tails accumulate and
            // beat phasors stay locked while the well is closed — it opens warm.
            .add_systems(Update, live::ingest_live_events)
            // Well-only per-frame work. One long sequence, split into two
            // chained groups only because Bevy's tuple impls cap out at 20:
            // input + polls + reconcile first, then the per-frame render sync.
            .add_systems(
                Update,
                (
                    (
                        scene::well_keyboard,
                        sync::poll_clusters,
                        sync::apply_clusters,
                        rays::poll_tracks,
                        rays::apply_tracks,
                        rays::sync_track_rays,
                        rays::animate_track_rays,
                        sync::sync_time_well,
                        text::build_card_scenes,
                        text::update_reading_card,
                        text::build_horizon_label,
                    )
                        .chain(),
                    (
                        scene::spin_rings,
                        scene::move_cards_toward_target,
                        scene::ease_camera_to_focused_ring,
                        scene::billboard_cards,
                        scene::highlight_selection,
                        scene::highlight_lineage,
                        scene::highlight_drift,
                        scene::accumulate_ring_activity,
                        scene::tick_and_sync_rings,
                        scene::dim_nonfocused_rings,
                        live::sync_card_live_uniforms,
                        scene::sync_focus_card_visibility,
                        hud::position_well_hud,
                        hud::update_well_hud,
                    )
                        .chain(),
                )
                    .chain()
                    .run_if(in_state(Screen::TimeWell)),
            );
    }
}
