//! The horizon dive — a **search interface** over everything past the time
//! well's event horizon, presented as a space (`docs/horizon-dive.md`).
//!
//! The well seats 40 contexts on four rings and renders the rest as a "+N"
//! chip at its throat. Activating that chip drops you here: hundreds of
//! demoted / concluded / archived / overflowed contexts, each holding a
//! **stable** seat in an accretion stream, with a query line as the primary
//! input and directional keys that snap the selection between lit or linked
//! cards. There is no free flight, no cursor, and no full-graph render.
//!
//! Five principles, each mapped to the code that keeps it honest:
//!
//! 1. **Search-first, space-as-presentation.** The query line is the primary
//!    input; the camera is a consequence of the selection
//!    ([`scene::ease_dive_camera`]), never an input.
//! 2. **Stable positions, query as light.** [`layout::stream_coord`] takes no
//!    query — enforced by its signature. Lights change brightness and scale
//!    ([`scene::sync_card_visuals`]); nothing ever moves.
//! 3. **Local edge reveal.** [`layout::neighborhood`] returns the selection's
//!    immediate links and caps them; one mesh draws them
//!    ([`scene::sync_constellation`]).
//! 4. **Snap navigation.** [`layout::snap_neighbor`] picks the nearest
//!    candidate inside a screen-space cone. No aiming.
//! 5. **Cards, not stars.** Every context is a `WellCardMaterial` quad with
//!    an MSDF face — the well's own card language, reused wholesale.
//!
//! Module map (the split `time_well`/`fsn` also use — pure math out of Bevy's
//! way so the rules are unit-testable):
//! - [`corpus`] — the record, the synthetic dataset, and the search seam.
//! - [`layout`] — accretion-stream coordinates, snap navigation, edge reveal.
//! - [`text`] — MSDF glyphs for card faces and the query/reading HUD.
//! - [`scene`] — entities, camera, input consumption, per-frame sync.
//!
//! # Entry (debug only — this is a spike)
//!
//! `F2` from anywhere, or `h` while zoomed into the well. Both fire
//! `Action::DiveHorizon` through the central action table; nothing here reads
//! raw input except the query line's declared keyboard grab. `Esc` returns to
//! whichever screen you dived from.

pub mod corpus;
pub mod layout;
pub mod scene;
pub mod text;

use bevy::prelude::*;

use crate::ui::screen::Screen;

pub use scene::DiveTyping;

/// Wires the horizon dive into the app.
pub struct HorizonDivePlugin;

impl Plugin for HorizonDivePlugin {
    fn build(&self, app: &mut App) {
        // No `MaterialPlugin` registration here: the dive reuses
        // `WellCardMaterial`, which `TimeWellPlugin` already registers.
        // Registering it twice would be a second copy of the same pipeline.
        app.init_resource::<scene::HorizonDiveState>()
            .init_resource::<scene::DiveTyping>()
            .add_systems(OnEnter(Screen::HorizonDive), scene::enter_dive)
            .add_systems(OnExit(Screen::HorizonDive), scene::exit_dive)
            // Ungated: the entry action can fire from any screen, and it
            // consumes `ActionFired`, so it follows dispatch.
            .add_systems(
                Update,
                scene::handle_dive_entry.after(crate::input::InputPhase::Dispatch),
            )
            .add_systems(
                Update,
                (
                    // Input first: the query grab, then navigation actions.
                    // `dive_query_keys` reads the `GrabbedKey` stream
                    // `dispatch_input` writes, so it must follow dispatch —
                    // the group's `.after` covers both.
                    scene::dive_query_keys,
                    scene::dive_actions,
                    // Then the consequences, in causal order: lights, camera,
                    // orientation, materials, constellation, text.
                    scene::apply_query,
                    scene::ease_dive_camera,
                    scene::billboard_cards,
                    scene::sync_card_visuals,
                    scene::sync_constellation,
                    scene::build_card_text,
                    scene::sync_hud,
                )
                    .chain()
                    .after(crate::input::InputPhase::Dispatch)
                    .run_if(in_state(Screen::HorizonDive)),
            );
    }
}
