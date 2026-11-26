//! Camera positions for the room, score, switchboard, and Time Well.
//! The room eases its camera toward the eye and look point from [`resolve`].

use bevy::prelude::Vec3;

use super::switchboard;
use super::nav::Station;
use crate::view::time_well::card;
use crate::view::time_well::scene::{FOCUS_CARD_POS, STATION_CENTER_PLACEMENT, placement_to_room};

const OVERVIEW_POS: Vec3 = Vec3::new(0.0, 420.0, 2050.0);
const OVERVIEW_LOOK: Vec3 = Vec3::new(0.0, 90.0, 0.0);

/// Distance from the lamp grid to the camera when the switchboard is focused.
const SWITCHBOARD_STANDOFF: f32 = 850.0;

/// Offsets from the well center for its elevated overview.
const WELL_HERO_HEIGHT: f32 = 540.0;
const WELL_HERO_BACK: f32 = 900.0;

/// Camera distance in front of the well's focus card when focused (larger =
/// card fills less of the frame). Tuned a touch back so the focused card
/// isn't oversized.
const FOCUS_DOLLY: f32 = 430.0;

/// Back-off distance along the funnel axis for the well's ring-overview shot,
/// **as a multiple of the focused ring's radius**, so a bigger ring is framed
/// from proportionally further back (neighbor rings bleed off the top/bottom
/// edges).
const RING_CAM_BACK: f32 = 1.8;
/// Well-local lift of the well's ring-overview camera. Since the rings were
/// laid parallel to the floor, backing off along the gate normal keeps the eye
/// *exactly level* with the ring it frames (the normal lies in the ring plane,
/// and that plane is now horizontal) — so this constant no longer "raises the
/// camera back to level" as its old doc claimed: it is now the sole source of
/// the shot's look-down angle. At 450 local (× `STATION_CENTER_SCALE`) it
/// lifts the eye ≈183 world units over the gate, ≈30° down at the well's
/// framing distance. Higher = steeper look-down onto the ring.
/// **Amy-tunable** — and the first thing to reach for if the two rings, only
/// 75 world units apart in height now, crowd each other in frame.
const RING_CAM_LIFT: f32 = 450.0;
/// How far in front of the focused ring's center (along the axis, × radius)
/// the well's ring-overview look-point leads — 0 looks straight at the ring
/// plane.
const RING_CAM_LOOK_LEAD: f32 = 0.0;

// ── The shot table ───────────────────────────────────────────────────────────

/// Pure inputs the well's own camera-shot math needs ([`well_local_shot`]) —
/// the same two fields `time_well::scene::TimeWellState` carries, handed in
/// explicitly rather than this module reading a Bevy `Res` (keeping it pure,
/// same stance as `bearing.rs`): whether focused on the reading card (a
/// head-on dolly), or which ring to frame at the gate otherwise.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WellShotInput {
    pub focused: bool,
    pub focused_ring: usize,
}

/// One named camera shot the room can resolve a pose for — the "predefined,
/// tunable camera positions" table. [`resolve`] is the single entry point;
/// nothing else in the shell should synthesize a camera pose by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomShot {
    /// Pulled-back console view, framing the whole octagon — where the camera
    /// rests when no station is focused/zoomed onto a pose of its own.
    RoomOverview,
    /// Keep the well below the score, looking across the table toward the current bar.
    ScoreOverview,
    /// Level view of the lamp grid from the south side of the well.
    Switchboard,
    /// Focus a ring or reading card in well-local space.
    WellOverview(WellShotInput),
    /// The well's elevated establishing shot (`TimeWellState::hero`) — see
    /// [`WELL_HERO_HEIGHT`]/[`WELL_HERO_BACK`]'s own doc.
    WellHero,
}

impl RoomShot {
    /// Room-scale view of the focused station.
    pub fn focused(station: Station) -> Self {
        match station {
            Station::TimeWell => RoomShot::RoomOverview,
            Station::Switchboard => RoomShot::Switchboard,
        }
    }
}

/// Resolve a [`RoomShot`] to a room-space `(eye, look_at)` pose. Pure —
/// `super::ease_shell_camera` is the one caller, and does the exponential
/// ease itself; this function only answers "where."
pub fn resolve(shot: RoomShot) -> (Vec3, Vec3) {
    match shot {
        RoomShot::RoomOverview => (OVERVIEW_POS, OVERVIEW_LOOK),
        RoomShot::ScoreOverview => (Vec3::new(0.0, 395.0, 1100.0), Vec3::new(0.0, 220.0, -400.0)),
        RoomShot::Switchboard => {
            let look = switchboard::CENTER;
            (look - Vec3::Z * SWITCHBOARD_STANDOFF, look)
        }
        RoomShot::WellOverview(input) => {
            let (local_eye, local_look) = well_local_shot(input);
            (
                placement_to_room(&STATION_CENTER_PLACEMENT, local_eye),
                placement_to_room(&STATION_CENTER_PLACEMENT, local_look),
            )
        }
        RoomShot::WellHero => {
            let look = STATION_CENTER_PLACEMENT.translation;
            let eye = look + Vec3::new(0.0, WELL_HERO_HEIGHT, WELL_HERO_BACK);
            (eye, look)
        }
    }
}

/// The well's own local (pre-placement) camera pose — migrated unchanged
/// from `time_well::scene`'s old `ease_camera_to_focused_ring` system
/// (deleted once this math moved here): a head-on dolly onto the focus card
/// when [`WellShotInput::focused`], else a frame of the focused ring's gate
/// seat. Pure; [`resolve`] composes the result into room space through
/// `placement_to_room` — the well's own funnel math never needs to know it
/// now sits at the room's center instead of the world origin.
fn well_local_shot(input: WellShotInput) -> (Vec3, Vec3) {
    if input.focused {
        // Dolly to a head-on framing of the focus card so it dominates the view.
        return (FOCUS_CARD_POS + Vec3::new(0.0, 0.0, FOCUS_DOLLY), FOCUS_CARD_POS);
    }

    // Frame the focused ring. Its center rides the tilted funnel axis at the
    // ring's depth; the axis (tilt·+Z) points up-and-toward the camera.
    let band = kaijutsu_viz::layout::ALL_BANDS[input.focused_ring.min(card::N_BANDS - 1)];
    let (radius, depth) = card::band_ring(band);
    let tilt = card::well_tilt_quat();
    // Frame the GATE card face-on: sit out along the outward face-normal of
    // the seat the selected slide spins to (`card::GATE_ANGLE`), backed off ∝
    // radius and lifted, looking at the gate point. Whatever card is at the
    // gate reads face-on and roughly centered; the ring curves away behind it
    // (relief), and the shallower/deeper rings sit above/below by depth and
    // bleed off the edges.
    let a = card::GATE_ANGLE;
    let gate = tilt * Vec3::new(radius * a.cos(), radius * a.sin(), depth);
    let normal = tilt * Vec3::new(-a.sin(), a.cos(), 0.0); // gate slide's face normal
    let pos = gate + normal * (radius * RING_CAM_BACK) + Vec3::Y * RING_CAM_LIFT;
    let look = gate + Vec3::Y * RING_CAM_LOOK_LEAD;
    (pos, look)
}

/// Room-space world Y the camera would sit at for `ring`'s overview shot.
/// Pure helper for the floor-clearance test below (and readable enough to be
/// worth keeping around when the framing is next live-tuned).
#[cfg(test)]
fn well_ring_overview_eye(ring: usize) -> Vec3 {
    let input = WellShotInput { focused: false, focused_ring: ring };
    resolve(RoomShot::WellOverview(input)).0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn focused_shot_frames_console_from_the_overview() {
        let (pos, look) = resolve(RoomShot::focused(Station::TimeWell));
        assert_eq!(pos, OVERVIEW_POS);
        assert_eq!(look, OVERVIEW_LOOK);
    }

    #[test]
    fn focused_switchboard_frames_the_lamp_grid_from_inside_the_room() {
        let (eye, look) = resolve(RoomShot::focused(Station::Switchboard));
        assert_eq!(look, switchboard::CENTER);
        assert_eq!(eye.x, look.x);
        assert_eq!(eye.y, look.y);
        assert!(eye.z > super::super::KEEPOUT_RADIUS);
        assert!(eye.z < look.z);
    }

    #[test]
    fn every_camera_pose_stays_inside_the_vault_dome() {
        // Outside the dome the camera would face its near inner wall, occluding
        // the room. Every focus (overview + each bearing) must orbit within it.
        for s in Station::ALL {
            let (pos, _) = resolve(RoomShot::focused(s));
            assert!(
                pos.length() < super::super::DOME_RADIUS,
                "{s:?} camera at {} escapes the dome ({})",
                pos.length(),
                super::super::DOME_RADIUS
            );
        }
    }

    #[test]
    fn well_overview_differs_from_the_room_overview() {
        let input = WellShotInput { focused: false, focused_ring: 0 };
        assert_ne!(
            resolve(RoomShot::WellOverview(input)),
            resolve(RoomShot::RoomOverview)
        );
    }

    #[test]
    fn well_overview_focused_dollies_onto_the_focus_card_through_the_placement() {
        let input = WellShotInput { focused: true, focused_ring: 0 };
        let (eye, look) = resolve(RoomShot::WellOverview(input));
        let local_eye = FOCUS_CARD_POS + Vec3::new(0.0, 0.0, FOCUS_DOLLY);
        assert_eq!(eye, placement_to_room(&STATION_CENTER_PLACEMENT, local_eye));
        assert_eq!(look, placement_to_room(&STATION_CENTER_PLACEMENT, FOCUS_CARD_POS));
    }

    #[test]
    fn well_overview_unfocused_frames_the_focused_rings_gate_through_the_placement() {
        // The real production path (`room::ease_shell_camera`, which builds
        // `RoomShot::WellOverview` straight from `TimeWellState`) must match
        // the well-local pose composed through the station placement.
        for ring in 0..card::N_BANDS {
            let input = WellShotInput { focused: false, focused_ring: ring };
            let (eye, look) = resolve(RoomShot::WellOverview(input));
            let (local_eye, local_look) = well_local_shot(input);
            assert_eq!(eye, placement_to_room(&STATION_CENTER_PLACEMENT, local_eye));
            assert_eq!(look, placement_to_room(&STATION_CENTER_PLACEMENT, local_look));
        }
    }

    #[test]
    fn well_ring_overview_stands_above_its_ring_and_never_under_the_floor() {
        // Post-flatten (2026-08-01) the gate normal lies in a HORIZONTAL ring
        // plane, so backing off along it holds the eye level with the ring and
        // `RING_CAM_LIFT` alone supplies the look-down. Two things must hold
        // for every ring: the eye stays above the room floor (a camera under
        // the floor sees the underside of everything), and it stands above the
        // ring it is framing (looking DOWN onto the carousel, per the shot's
        // own doc), not below it looking up.
        for ring in 0..card::N_BANDS {
            let eye = well_ring_overview_eye(ring);
            let look = resolve(RoomShot::WellOverview(WellShotInput {
                focused: false,
                focused_ring: ring,
            }))
            .1;
            assert!(eye.y > 0.0, "ring {ring}: eye at y={} is under the room floor", eye.y);
            assert!(
                eye.y > look.y,
                "ring {ring}: eye ({}) must sit above the gate it frames ({})",
                eye.y,
                look.y
            );
        }
    }

    #[test]
    fn well_overview_composes_through_a_non_identity_placement() {
        // Confirms the composition is real (not accidentally bypassed): the
        // placement's translation/scale must actually show up in the result,
        // not just pass the local pose straight through.
        let input = WellShotInput { focused: true, focused_ring: 0 };
        let (eye, _) = resolve(RoomShot::WellOverview(input));
        let unplaced = FOCUS_CARD_POS + Vec3::new(0.0, 0.0, FOCUS_DOLLY);
        assert_ne!(eye, unplaced, "the placement must translate/scale the well's local pose");
    }
}
