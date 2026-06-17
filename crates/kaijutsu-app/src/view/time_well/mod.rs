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

pub mod card;
pub mod scene;
pub mod sync;
pub mod text;

use bevy::prelude::*;

use crate::ui::screen::Screen;

/// Wires the time-well browser into the app.
pub struct TimeWellPlugin;

impl Plugin for TimeWellPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<scene::TimeWellState>()
            .add_systems(OnEnter(Screen::TimeWell), scene::enter_time_well)
            .add_systems(OnExit(Screen::TimeWell), scene::exit_time_well)
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
                    sync::apply_block_status,
                    text::build_card_scenes,
                    text::update_reading_card,
                    scene::move_cards_toward_target,
                    scene::billboard_cards,
                    scene::highlight_selection,
                    scene::highlight_lineage,
                )
                    .chain()
                    .run_if(in_state(Screen::TimeWell)),
            );
    }
}
