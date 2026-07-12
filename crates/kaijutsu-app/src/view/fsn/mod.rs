//! The FSN landscape — the VFS-as-terrain world behind the room's N archway
//! ("DATA HORIZON"), slice 0 (`docs/scenes/vfs.md`): quadtree layout +
//! hash-seeded relaxed-Voronoi fields rendered as line-list wireframe +
//! vertex points, three LOD tiers live, fly + select only.
//!
//! Module map, mirroring `time_well`'s split:
//! - [`layout`] — pure math: VFS-path → world-space placement, the
//!   height-channel mapping, wireframe/point mesh vertex builders, the LOD
//!   tier decision, camera-fly clamps. No Bevy types, unit-tested.
//! - [`sync`] — data: the `vfs_snapshot` poll → [`sync::FsnState`] drain
//!   (the enumeration-on-demand scheduler, vfs.md claim 3).
//! - [`scene`] — the 3D scene: spawn/despawn, the fly camera, per-directory
//!   field mesh entities, LOD-tier visibility gating, selection.
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
//! well/patch-bay furniture.

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
            .add_systems(OnEnter(Screen::Fsn), scene::enter_fsn)
            .add_systems(OnExit(Screen::Fsn), scene::exit_fsn)
            .add_systems(
                Update,
                (
                    sync::poll_fsn_snapshot,
                    sync::apply_fsn_snapshot,
                    scene::sync_fsn_fields,
                    scene::fsn_camera_fly,
                    scene::fsn_select,
                    scene::apply_fsn_lod,
                    scene::fsn_keyboard,
                )
                    .chain()
                    .run_if(in_state(Screen::Fsn)),
            );
    }
}
