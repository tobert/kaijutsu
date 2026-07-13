//! Room camera-shot table (time-well/room integration plan, Slice A,
//! 2026-07-11): every named camera pose the shell resolves to. Pure — `Vec3`
//! math in, `(eye, look_at)` pose out, no Bevy `World`/system access, same
//! stance as `bearing.rs`. `super::ease_shell_camera` reads
//! [`super::RoomState::zoomed`]/the focused station each frame, builds a
//! [`RoomShot`], and eases the shared `Camera3d` toward whatever [`resolve`]
//! returns — the exponential tween itself (`super::CAMERA_EASE_RATE`,
//! `lerp`/`slerp`) stays there; this module only answers "where," never "how
//! fast."
//!
//! Moved wholesale from the old `desired_camera`/`fullscreen_pose` free
//! functions in `room/mod.rs` — same math, same constants, zero behaviour
//! change. The move exists for what a LATER slice needs, not anything this
//! one changes: the time well sits at the room's CENTER with no wall bearing
//! at all (`bearing::focus_dir(Station::TimeWell) == None`), so it can never
//! resolve through [`RoomShot::Fullscreen`] the way a wall station's panel
//! does — that variant's `.expect()` would panic. [`RoomShot::WellOverview`]
//! was Slice A's stub (returning the bare room overview); Slice C
//! (`lovely-swimming-prism.md`, 2026-07-11) fills it with the well's own
//! dolly-to-focused-ring / dolly-to-focus-card math, migrated unchanged from
//! `time_well::scene::ease_camera_to_focused_ring` (deleted as a Bevy system
//! once its math moved here) and composed into room space through
//! [`STATION_CENTER_PLACEMENT`] — the well's own local camera math never
//! needs to know it now sits at the room's center instead of the world
//! origin.

use bevy::prelude::Vec3;

use super::bearing;
use super::nav::Station;
use crate::view::palette;
use crate::view::time_well::card;
use crate::view::time_well::scene::{FOCUS_CARD_POS, STATION_CENTER_PLACEMENT, placement_to_room};

// ── Camera-framing constants (Amy-tunable) ──────────────────────────────────
// `super::ROOM_RADIUS` / `super::WALL_HEIGHT` stay in `room/mod.rs` — they're
// shared with non-camera geometry (wall panels, floor traces, marker pylons)
// — and are read here via `super::`.

/// The console (TimeWell) overview pose — pulled back from the south, framing
/// the whole octagon so every bearing's ambient glow reads at once. Moved
/// unchanged from `room/mod.rs`'s old `OVERVIEW_POS`/`OVERVIEW_LOOK`.
/// **Amy-tunable.**
const OVERVIEW_POS: Vec3 = Vec3::new(0.0, 420.0, 2050.0);
const OVERVIEW_LOOK: Vec3 = Vec3::new(0.0, 90.0, 0.0);

/// Approach-pose standoff: how far back from the focused wall panel's FACE
/// the eye stands (retuned 2026-07-13, Amy: "out enough to see a bit of
/// neighboring walls and the whole wall being viewed" — the old close-in
/// pose at eye radius 160 looking down at marker height showed a wall
/// fragment at a tilt). The whole-height pinhole distance for a 560-tall
/// panel at [`CAMERA_FOV_Y`] is ≈676 ([`fullscreen_pose`]'s own math);
/// standing 850 back clears that with margin, so the full panel height sits
/// in frame with breathing room and — at any real window aspect — the
/// horizontal frustum spills past the panel's ~994 width onto the
/// neighboring faces. Eye radius from center = `WALL_APOTHEM − 850` = 350,
/// comfortably outside [`super::KEEPOUT_RADIUS`], still on the focus side.
/// **Amy-tunable.**
const APPROACH_PANEL_STANDOFF: f32 = 850.0;

/// Approach-pose eye height AND look height: the panel's own vertical
/// center (the same number [`palette::STATION_W_MOUNT_Y`] names for the
/// wheel's mount). Eye and look sharing a height is the point: the camera
/// flies LEVEL between panels, no down-tilt (the pre-2026-07-13 pose looked
/// from 260 down to 130). Derived from `WALL_HEIGHT`, so it follows any
/// wall retune. **Amy-tunable** (as an offset choice, not a raw number).
const APPROACH_EYE_HEIGHT: f32 = super::WALL_HEIGHT * 0.5;

/// The well's **hero pose** (Amy: "a final up arrow took the camera to focus
/// on the well from a bit above looking down... esp with the room to give it
/// perspective") — an elevated, pulled-back establishing shot of the well
/// framed against the room around it, reached by pressing Up at the mouth
/// ring (nowhere further inward to focus, `TimeWellState::hero`). Deliberately
/// a fixed ROOM-space pose, not composed through [`STATION_CENTER_PLACEMENT`]'s
/// rotation the way [`well_local_shot`] is — the point is a stable, room-
/// oriented 3/4 view that reads the same regardless of how the well's own
/// internal geometry is tuned, catching the octagon's walls/floor behind it
/// for context the tight ring-gate dive can't give. Height/back-off are
/// offsets from [`STATION_CENTER_PLACEMENT`]'s own translation, so retuning
/// where the well sits doesn't silently stale this shot. **First guess — live
/// -tune over BRP**, same as every other Amy-tunable constant here.
const WELL_HERO_HEIGHT: f32 = 540.0;
const WELL_HERO_BACK: f32 = 900.0;

/// The camera's vertical field of view — mirrors `main::setup_camera`'s
/// `Camera3d::default()` projection (Bevy's own `PerspectiveProjection`
/// default, `fov: PI / 4.0`). [`fullscreen_pose`] derives its standoff
/// distance from this; if the app camera's FOV ever changes, this constant
/// must follow it or the fullscreen pose stops framing a panel edge-to-edge.
/// `fullscreen_pose_fills_the_frame_with_the_panel_height` locks the
/// relationship instead of trusting two hand-written copies to agree.
const CAMERA_FOV_Y: f32 = std::f32::consts::FRAC_PI_4;

// ── The well's own camera-framing constants (Slice C — moved unchanged from
// `time_well/scene.rs`'s old `ease_camera_to_focused_ring`, deleted as a
// system once this math moved here) ─────────────────────────────────────────

/// Camera distance in front of the well's focus card when focused (larger =
/// card fills less of the frame). Tuned a touch back so the focused card
/// isn't oversized.
const FOCUS_DOLLY: f32 = 430.0;

/// Back-off distance along the funnel axis for the well's ring-overview shot,
/// **as a multiple of the focused ring's radius**, so a bigger ring is framed
/// from proportionally further back (neighbor rings bleed off the top/bottom
/// edges).
const RING_CAM_BACK: f32 = 1.8;
/// World-Y lift of the well's ring-overview camera. With the gate-normal
/// framing the gate card's face points down-and-forward, so the normal
/// back-off pulls the camera below the gate; this lift raises it back to
/// roughly level / gently looking down. Higher = steeper look-down onto the
/// ring. **Amy-tunable.**
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
    /// Room-scale pose for a wall station: standing on the SAME side of the
    /// room as its bearing, between the console and the wall, looking out at
    /// its marker/nameplate (the old `desired_camera`'s `Some` branch).
    Approach(Station),
    /// Camera fills the frame with `station`'s wall panel — "diving IS
    /// fullscreening a panel." Only valid for a station with a wall bearing;
    /// [`resolve`] panics otherwise. Today the sole such station,
    /// `Station::PatchBay`, is the only one ever passed here.
    Fullscreen(Station),
    /// The well's own dolly pose (Slice C) — composed through
    /// [`STATION_CENTER_PLACEMENT`] since the well has no wall bearing at all
    /// (that's the whole reason this variant exists rather than reusing
    /// `Fullscreen`, which would panic for it). See [`well_local_shot`] for
    /// the local (pre-placement) math, migrated from the old
    /// `ease_camera_to_focused_ring`.
    WellOverview(WellShotInput),
    /// The well's elevated establishing shot (`TimeWellState::hero`) — see
    /// [`WELL_HERO_HEIGHT`]/[`WELL_HERO_BACK`]'s own doc.
    WellHero,
}

impl RoomShot {
    /// Which room-scale shot `station` resolves to at rest (not zoomed): the
    /// overview if it has no wall bearing, else its approach pose. Promotes
    /// the old `desired_camera`'s own top-level branch to a constructor so a
    /// caller doesn't have to re-derive which case applies.
    pub fn focused(station: Station) -> Self {
        match bearing::focus_dir(station) {
            None => RoomShot::RoomOverview,
            Some(_) => RoomShot::Approach(station),
        }
    }
}

/// Resolve a [`RoomShot`] to a room-space `(eye, look_at)` pose. Pure —
/// `super::ease_shell_camera` is the one caller, and does the exponential
/// ease itself; this function only answers "where."
pub fn resolve(shot: RoomShot) -> (Vec3, Vec3) {
    match shot {
        RoomShot::RoomOverview => (OVERVIEW_POS, OVERVIEW_LOOK),
        RoomShot::Approach(station) => approach_pose(station),
        RoomShot::Fullscreen(station) => {
            let dir = bearing::focus_dir(station).expect(
                "RoomShot::Fullscreen is only constructed for a station with a wall bearing to fill the frame with",
            );
            // Every panel shares the same vertical center (`super::WALL_HEIGHT
            // * 0.5` — `palette::STATION_W_MOUNT_Y` is this same number, named
            // for the wheel's own placement contract); a future second
            // zoomable station reads this general height too, not a
            // station-specific one.
            fullscreen_pose(Vec3::from_array(dir), super::WALL_HEIGHT * 0.5)
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

/// The **approach** pose for a wall station: a LEVEL wall-viewing shot
/// (retuned 2026-07-13 — Amy: see the whole wall being viewed plus a bit of
/// its neighbors, then Enter fullscreens it). Eye and look share
/// [`APPROACH_EYE_HEIGHT`] (the panel's vertical center) so flipping
/// between panels is a level swing with no down-tilt; the eye stands
/// [`APPROACH_PANEL_STANDOFF`] back from the panel face — far enough that
/// the whole panel height fits the frustum with margin and the horizontal
/// frustum catches slivers of the neighboring faces — on the SAME side of
/// the room as the bearing (still "walk toward the station you're
/// studying"; the eye never crosses center, so the console never enters the
/// sight line).
///
/// The old per-station look-point exceptions (marker radius vs wall
/// apothem, furniture height vs mount height) dissolve: the wall PANEL is
/// the subject now, and every panel stands at [`palette::WALL_APOTHEM`]
/// with its center at the same height. Panics if `station` has no wall
/// bearing — only [`RoomShot::focused`]/[`RoomShot::Approach`] should ever
/// reach this, and `focused` never picks `Approach` for a bearing-less
/// station.
fn approach_pose(station: Station) -> (Vec3, Vec3) {
    let d = bearing::focus_dir(station)
        .expect("RoomShot::Approach is only constructed for a station with a wall bearing");
    let eye_r = palette::WALL_APOTHEM - APPROACH_PANEL_STANDOFF;
    (
        Vec3::from_array(bearing::approach_camera(d, eye_r, APPROACH_EYE_HEIGHT)),
        Vec3::from_array(bearing::approach_look(d, palette::WALL_APOTHEM, APPROACH_EYE_HEIGHT)),
    )
}

/// The room-space `(eye, look-at)` that fills the camera's vertical frustum
/// with exactly one wall panel — "diving IS fullscreening a panel" (Amy,
/// 2026-07-10 evening). `bearing_dir` is the panel's bearing (unit XZ,
/// pointing from center OUT to the wall, e.g. [`bearing::Bearing::West`]'s);
/// `mount_y` is the panel's own vertical center (every panel shares
/// `super::WALL_HEIGHT * 0.5` — see [`resolve`]'s `Fullscreen` arm).
///
/// The eye stands on the panel's inward normal — i.e. back toward center,
/// along `-bearing_dir` — at the pinhole distance `d` that makes the panel's
/// full height exactly subtend the frustum: the standard "fit height in
/// frame" relation `tan(FOV_Y / 2) = (height / 2) / d`, solved for `d`. Pure;
/// unit-tested against that formula, against standing inside the octagon
/// (not through the wall), and against looking at the panel's own center.
fn fullscreen_pose(bearing_dir: Vec3, mount_y: f32) -> (Vec3, Vec3) {
    let d = (super::WALL_HEIGHT * 0.5) / (CAMERA_FOV_Y * 0.5).tan();
    let panel = Vec3::new(
        bearing_dir.x * palette::WALL_APOTHEM,
        mount_y,
        bearing_dir.z * palette::WALL_APOTHEM,
    );
    let eye = panel - bearing_dir * d;
    (eye, panel)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- fullscreen_pose (the "diving IS fullscreening a panel" math) --

    #[test]
    fn fullscreen_pose_stands_the_pinhole_distance_back_from_the_panel() {
        let dir = Vec3::from_array(bearing::Bearing::West.dir());
        let (eye, look) = fullscreen_pose(dir, palette::STATION_W_MOUNT_Y);
        let d = (super::super::WALL_HEIGHT * 0.5) / (CAMERA_FOV_Y * 0.5).tan();
        assert!(
            ((look - eye).length() - d).abs() < 1e-3,
            "eye should stand exactly d={d} back from the panel, got {}",
            (look - eye).length()
        );
        assert!((look.y - palette::STATION_W_MOUNT_Y).abs() < 1e-5, "looks at the given mount height");
    }

    #[test]
    fn fullscreen_pose_looks_squarely_at_the_panel_center() {
        let dir = Vec3::from_array(bearing::Bearing::West.dir());
        let (_, look) = fullscreen_pose(dir, palette::STATION_W_MOUNT_Y);
        let look_r = look.x * dir.x + look.z * dir.z;
        assert!(
            (look_r - palette::WALL_APOTHEM).abs() < 1e-3,
            "look point should sit exactly on the wall apothem: {look_r}"
        );
    }

    #[test]
    fn fullscreen_pose_stands_inside_the_octagon_not_through_the_wall() {
        let dir = Vec3::from_array(bearing::Bearing::West.dir());
        let (eye, _) = fullscreen_pose(dir, palette::STATION_W_MOUNT_Y);
        let eye_r = eye.x * dir.x + eye.z * dir.z;
        assert!(eye_r > 0.0, "eye stands on the room side of center: {eye_r}");
        assert!(eye_r < palette::WALL_APOTHEM, "eye stands short of the wall, inside the octagon: {eye_r}");
    }

    #[test]
    fn fullscreen_shot_resolves_through_the_patch_bay_bearing() {
        // The real production path: `ease_shell_camera` builds
        // `RoomShot::Fullscreen(station)` straight from `RoomState::zoomed`,
        // not a pre-extracted direction. Lock that it matches the primitive
        // above for the one station that ever reaches it today.
        let via_shot = resolve(RoomShot::Fullscreen(Station::PatchBay));
        let via_primitive = fullscreen_pose(
            Vec3::from_array(bearing::Bearing::West.dir()),
            palette::STATION_W_MOUNT_Y,
        );
        assert_eq!(via_shot, via_primitive);
    }

    #[test]
    #[should_panic(expected = "wall bearing")]
    fn fullscreen_shot_panics_for_a_station_with_no_wall_bearing() {
        // The reason this table exists at all: `Station::TimeWell` has no
        // wall bearing (`bearing::focus_dir(TimeWell) == None`), so it can
        // never go through `Fullscreen` — the well needs `WellOverview`
        // instead. This locks the guard so a future caller can't silently
        // wire the well through the wrong variant and hit this panic live.
        let _ = resolve(RoomShot::Fullscreen(Station::TimeWell));
    }

    // -- RoomShot::focused / desired-camera math (moved from `room/mod.rs`'s
    //    old `desired_camera_*` tests, same expected values) --

    #[test]
    fn focused_shot_frames_console_from_the_overview() {
        let (pos, look) = resolve(RoomShot::focused(Station::TimeWell));
        assert_eq!(pos, OVERVIEW_POS);
        assert_eq!(look, OVERVIEW_LOOK);
    }

    #[test]
    fn focused_shot_approaches_the_tracks_wall_from_the_same_side() {
        // Tracks is East (+X). The camera stands on the SAME side as the
        // focus — walking toward the station, not sitting on the opposite
        // wall staring back through the console and the (occluding) west
        // pylon.
        let (pos, look) = resolve(RoomShot::focused(Station::Tracks));
        assert!(pos.x > 0.0, "camera stands on the same (east) side: {pos:?}");
        assert!(pos.x < super::super::ROOM_RADIUS, "the eye stops well short of the wall: {pos:?}");
        assert!(look.x > pos.x, "looks further east, out toward the wall: {look:?}");
        assert_eq!(pos.y, APPROACH_EYE_HEIGHT);
    }

    #[test]
    fn approach_pose_is_level_and_frames_the_whole_panel_with_margin() {
        // The 2026-07-13 retune, in two invariants:
        // (1) LEVEL — eye and look share a height, so flipping between
        //     panels never tilts down (the old pose looked from 260 to 130).
        // (2) WHOLE WALL — the eye stands further back than the pinhole
        //     distance that exactly fits the panel height, so the full
        //     560-tall panel subtends less than the vertical FOV and the
        //     horizontal frustum has spill left over for neighbor slivers.
        let fit_d = (super::super::WALL_HEIGHT * 0.5) / (CAMERA_FOV_Y * 0.5).tan();
        for s in [Station::PatchBay, Station::Tracks, Station::Vfs, Station::Radiators] {
            let (pos, look) = resolve(RoomShot::focused(s));
            assert_eq!(pos.y, look.y, "{s:?}: the approach must be level, no down-tilt");
            let d = (look - pos).length();
            assert!(
                d > fit_d,
                "{s:?}: eye at {d} must stand beyond the whole-height fit distance {fit_d}"
            );
        }
    }

    #[test]
    fn every_wall_station_approach_clears_the_console_with_the_look_point_farther_out() {
        // The core of the fix: eye and look both sit on the focus side, past
        // the console keep-out, with the look point farther out than the eye
        // — the console can never fall in the sight line between them (the
        // occlusion bug this pose replaces).
        for s in [Station::PatchBay, Station::Tracks, Station::Vfs, Station::Radiators] {
            let (pos, look) = resolve(RoomShot::focused(s));
            let d = bearing::focus_dir(s).expect("wall station has a bearing");
            let eye_r = pos.x * d[0] + pos.z * d[2];
            let look_r = look.x * d[0] + look.z * d[2];
            assert!(eye_r > super::super::KEEPOUT_RADIUS, "{s:?} eye clears the console keep-out: {eye_r}");
            assert!(look_r > eye_r, "{s:?} look point sits farther out than the eye: eye={eye_r} look={look_r}");
        }
    }

    #[test]
    fn every_wall_station_approach_looks_at_its_panel_center() {
        // The 2026-07-13 retune dissolved the old per-station look-point
        // exceptions (marker radius vs apothem, furniture vs mount height):
        // the wall PANEL is the subject now, and every panel stands at the
        // apothem with its center at WALL_HEIGHT × 0.5.
        for s in [Station::PatchBay, Station::Tracks, Station::Vfs, Station::Radiators] {
            let (_, look) = resolve(RoomShot::focused(s));
            let d = bearing::focus_dir(s).unwrap();
            let look_r = look.x * d[0] + look.z * d[2];
            assert!(
                (look_r - palette::WALL_APOTHEM).abs() < 1e-3,
                "{s:?} should look at the wall apothem: {look_r}"
            );
            assert!(
                (look.y - super::super::WALL_HEIGHT * 0.5).abs() < 1e-3,
                "{s:?} should look at the panel's vertical center: {}",
                look.y
            );
        }
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

    // -- RoomShot::WellOverview (Slice C: real math replaces the Slice A stub) --
    //
    // The old canary here (`well_overview_stub_matches_the_room_overview_
    // until_slice_c`) asserted equality with `RoomShot::RoomOverview` and
    // documented that Slice C landing real math should make it start
    // FAILING — it now does; these tests replace it.

    #[test]
    fn well_overview_no_longer_matches_the_bare_room_overview() {
        let input = WellShotInput { focused: false, focused_ring: 0 };
        assert_ne!(
            resolve(RoomShot::WellOverview(input)),
            resolve(RoomShot::RoomOverview),
            "Slice C's real well math must diverge from the old stub"
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
        // the pure primitive it composes, the same "via_shot == via_primitive"
        // lock `fullscreen_shot_resolves_through_the_patch_bay_bearing` uses.
        for ring in 0..card::N_BANDS {
            let input = WellShotInput { focused: false, focused_ring: ring };
            let (eye, look) = resolve(RoomShot::WellOverview(input));
            let (local_eye, local_look) = well_local_shot(input);
            assert_eq!(eye, placement_to_room(&STATION_CENTER_PLACEMENT, local_eye));
            assert_eq!(look, placement_to_room(&STATION_CENTER_PLACEMENT, local_look));
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
