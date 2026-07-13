//! The FSN landscape — the VFS-as-terrain world behind the room's N archway
//! ("DATA HORIZON"), slice 0 (`docs/scenes/vfs.md`): quadtree layout +
//! hash-seeded relaxed-Voronoi fields rendered as line-list wireframe +
//! vertex points, three LOD tiers live, fly + select only. Slice 1 (lane A,
//! "the ambient world") layers on top: baked recency glow, churn heat, the
//! orbiting vessel, and a Room-side backdrop glimpse — none of it changes
//! the slice-0 shape above, only what colors/brightens the same geometry.
//! Retuned 2026-07-13: the vessel IS the octagon room, circling the world
//! on the same orbit the room's N portal renders from (diving is
//! de-emphasized; the portal is the primary FSN surface).
//!
//! Module map, mirroring `time_well`'s split:
//! - [`layout`] — pure math: VFS-path → world-space placement, the
//!   height-channel mapping, wireframe/point mesh vertex builders, the LOD
//!   tier decision, camera-fly clamps, recency weight, the vessel
//!   silhouette, and the shared orbit pose (portal camera + visible
//!   vessel). No Bevy types, unit-tested.
//! - [`sync`] — data: the `vfs_snapshot` poll → [`sync::FsnState`] drain
//!   (the enumeration-on-demand scheduler, vfs.md claim 3).
//! - [`heat`] — data: per-path churn heat (decaying, ancestor-attenuated),
//!   self-contained pending the digest ingest system a parallel lane wires
//!   in as a follow-up (see that module's own doc).
//! - [`scene`] — the 3D scene: spawn/despawn, the fly camera, per-directory
//!   field mesh entities (now recency-tinted), LOD-tier visibility gating
//!   (now heat-lifted), selection, and the orbiting vessel.
//! - [`backdrop`] — the Room-side glimpse, and since 2026-07-13 the PRIMARY
//!   FSN surface: an off-screen camera + one panel-spanning portal quad
//!   rendering the world's recency-tinted, heat-lifted wireframe districts
//!   while `Screen::Room` is live, no dive required — one fixed tier, no
//!   LOD swap, no selection.
//!
//! # Entry
//!
//! Reached from `Screen::Room`'s N bearing (`Station::Vfs`, "DATA HORIZON"):
//! Enter/Down transitions straight to `Screen::Fsn` (`room::room_keyboard`'s
//! own dive-through branch) rather than setting `RoomState::zoomed` — the
//! landscape is an unbounded world, too big to stand as room furniture
//! (`docs/scenes/shell.md`: "N stays a dive-THROUGH door"). The world itself
//! spawns on first entry (`scene::enter_fsn`, `OnEnter(Screen::Fsn)`) and
//! despawns on Esc back to the room (`scene::exit_fsn`, `OnExit(Screen::Fsn)`)
//! — never resident while the room itself is merely open, unlike the
//! well/patch-bay furniture. The [`backdrop`] glimpse is the opposite
//! lifetime: resident exactly while `Screen::Room` is, gone in `Screen::Fsn`
//! (no reason to render an off-screen impression of the world while
//! standing IN it).
//!
//! # Heat ingest
//!
//! `heat::ingest_vfs_activity` drains the kernel's activity digests
//! (`ServerEvent::VfsActivity`, subscribed during bootstrap —
//! `connection::actor_plugin`) into `FsnHeat` and the room's North bearing.
//! Ungated, like the decay tick: heat accumulates and cools on every screen.

pub mod backdrop;
pub mod heat;
pub mod layout;
pub mod scene;
pub mod sync;

use bevy::prelude::*;

use crate::ui::screen::Screen;

pub struct FsnPlugin;

impl Plugin for FsnPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<sync::FsnState>()
            .init_resource::<scene::FsnFields>()
            .init_resource::<scene::FsnSelection>()
            .init_resource::<heat::FsnHeat>()
            .init_resource::<backdrop::FsnBackdrop>()
            .add_systems(OnEnter(Screen::Fsn), scene::enter_fsn)
            .add_systems(OnExit(Screen::Fsn), scene::exit_fsn)
            .add_systems(OnEnter(Screen::Room), backdrop::spawn_backdrop)
            .add_systems(OnExit(Screen::Room), backdrop::despawn_backdrop)
            // UNGATED, unlike everything below: a vfs_snapshot reply must
            // settle `FsnState`'s in-flight slot even when the player Esc'd
            // out of the world before it landed. Bevy messages expire after
            // two frames — a screen-gated reader would silently drop that
            // reply, leaving `in_flight` set forever and wedging the fetch
            // queue on the next dive. Same "opens warm" reasoning as
            // `live::ingest_live_events` in the well; ingesting a reply on
            // another screen is free (it only writes the cache).
            .add_systems(Update, sync::apply_fsn_snapshot)
            // ALSO ungated (moved out of the `Screen::Fsn` chain below,
            // slice 1): the backdrop (`Screen::Room`) needs `FsnState`'s own
            // fetch queue to drain too, so the N windows can populate from a
            // cold start without ever diving through the door. Safe to run
            // on every screen: `FsnState::request`/`take_next_request` are
            // edge-triggered and single-flight (that module's own doc), so
            // an ungated poll is simply idle whenever nothing is queued —
            // it doesn't hot-loop or refetch anything extra by running here.
            .add_systems(Update, sync::poll_fsn_snapshot.after(sync::apply_fsn_snapshot))
            // Heat: digest ingest then ambient decay, both ungated — digests
            // arrive on every screen (that's the ambient point: the archway
            // and windows warm while you sit in the room), and a churn storm
            // should keep cooling whether or not the player is currently
            // looking at the FSN world. Chained so a digest recorded this
            // frame decays from the very next tick, never the same one.
            .add_systems(
                Update,
                (heat::ingest_vfs_activity, heat::tick_fsn_heat).chain(),
            )
            .add_systems(
                Update,
                (
                    scene::sync_fsn_fields,
                    scene::fsn_camera_fly,
                    scene::fsn_select,
                    scene::apply_fsn_lod,
                    scene::orbit_ship,
                    scene::sync_ship_glow,
                    scene::fsn_keyboard,
                )
                    .chain()
                    .run_if(in_state(Screen::Fsn)),
            )
            .add_systems(
                Update,
                (
                    backdrop::orbit_backdrop_camera,
                    backdrop::sync_backdrop_fields,
                    backdrop::sync_backdrop_heat,
                )
                    .chain()
                    .run_if(in_state(Screen::Room)),
            );
    }
}
