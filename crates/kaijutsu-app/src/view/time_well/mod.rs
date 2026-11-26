//! A radial context browser on the room table. Active and Recent seat up to
//! ten contexts each. The count on the deck includes contexts beyond the rings.
//! Track rays carry the score clocks. A completed turn sends a ripple toward
//! the current bearing of the context that played it.

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
/// Tails and beat clocks update on every screen. The room shows the same
/// cards, track rays and deck at either camera distance. Diving into the
/// well enables browsing and builds the text at reading size.
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
            .init_resource::<live::WellBeatsRes>()
            .init_resource::<rays::WellTracks>()
            // Go-to-well runs on every screen (prefix/gamepad reach it from
            // anywhere); it consumes ActionFired, so it follows dispatch.
            .add_systems(
                Update,
                scene::handle_go_to_well.after(crate::input::InputPhase::Dispatch),
            )
            // Fully ungated: opens warm on every screen, well included.
            .add_systems(
                Update,
                (
                    live::ingest_live_events,
                    rays::apply_tracks,
                    scene::tick_ring_activity,
                    scene::ripple_on_finished_turn.after(scene::tick_ring_activity).before(scene::sync_deck_material),
                ),
            )
            // Ambient: room-scale truth. The well breathes here whether
            // you're dived into it or just walking past its bearing.
            .add_systems(
                Update,
                // Split the ordered room systems to fit Bevy’s tuple limit.
                (
                    (
                        sync::poll_clusters,
                        sync::apply_clusters,
                        rays::poll_tracks,
                        rays::sync_track_rays,
                        rays::animate_track_rays,
                        sync::sync_time_well,
                        scene::spin_rings,
                        scene::move_cards_toward_target,
                        scene::billboard_cards,
                        scene::sync_well_palette,
                        scene::sync_deck_material,
                        live::sync_card_live_uniforms,
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
                        // tier, not dived-only — it must react to BOTH
                        // transitions (dismissing the legend on zoom-OUT, not
                        // just leaving it be on zoom-in), so it has to keep
                        // running at room scale even while unzoomed.
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
