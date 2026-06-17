//! Time-well context browser — the first concrete consumer of `kaijutsu-viz`.
//!
//! A radial 3D "well" of context cards: radius encodes lifecycle band (hot rim →
//! recent-concluded → haystack core), angle encodes within-band position. See
//! `docs/viz-substrate.md` and `docs/time-well-concepts.md` for the design.
//!
//! Module map:
//! - [`card`] — pure `ContextInfo` → card-model mapping, band assignment, layout,
//!   and the `LayoutPos → Vec3` well-lift (no Bevy, unit-tested).
//! - [`scene`] — the 3D scene: camera, root, screen toggle, billboarding, card
//!   motion, components, and live state resource.
//! - [`sync`] — the layout tick: keyed-join reconcile → spawn/despawn → layout.
//! - [`activity`] — the well's pulse: kernel-event stream → ring energy + ripples
//!   driving the base ring deck (unit-tested math).

pub mod activity;
pub mod card;
pub mod hud;
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
            .init_resource::<scene::TimeWellState>()
            .init_resource::<activity::RingActivity>()
            .add_systems(
                OnEnter(Screen::TimeWell),
                (scene::enter_time_well, hud::spawn_well_hud),
            )
            .add_systems(
                OnExit(Screen::TimeWell),
                (scene::exit_time_well, hud::despawn_well_hud),
            )
            // The toggle runs in every screen (it decides based on current state).
            .add_systems(Update, scene::toggle_time_well)
            // Well-only per-frame work.
            .add_systems(
                Update,
                (
                    scene::well_keyboard,
                    sync::poll_clusters,
                    sync::apply_clusters,
                    sync::sync_time_well,
                    text::build_card_scenes,
                    text::update_reading_card,
                    scene::move_cards_toward_target,
                    scene::ease_camera_to_selection,
                    scene::billboard_cards,
                    scene::highlight_selection,
                    scene::highlight_lineage,
                    scene::highlight_drift,
                    scene::accumulate_ring_activity,
                    scene::tick_and_sync_rings,
                    scene::sync_focus_card_visibility,
                    hud::update_well_hud,
                )
                    .chain()
                    .run_if(in_state(Screen::TimeWell)),
            );
    }
}
