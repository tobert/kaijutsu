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
//! is the stub a later slice fills with the well's own dolly-to-ring math;
//! for now it just returns the room overview, so the variant can be wired up
//! ahead of time without ever routing the well through `Fullscreen`.

use bevy::prelude::Vec3;

use super::bearing;
use super::nav::Station;
use crate::view::palette;

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

/// Approach-pose eye radius: how far out from center the camera stands when
/// facing a wall station — between the console and the wall, on the SAME
/// side as the focus ("walk toward the station you're studying", never sit
/// across the room staring back through the console and the diametrically
/// opposite pylon).
const ROOM_CAM_APPROACH_R: f32 = 160.0;
/// Approach-pose eye height — a comfortable "person standing" height.
const ROOM_CAM_APPROACH_HEIGHT: f32 = 260.0;

/// Where the approach pose *looks* (world-Y at the wall) by default:
/// furniture height, not plate height — the station's instrument is the
/// subject, the plate hangs above it in frame. Two stations override this
/// (see [`approach_pose`]).
const APPROACH_LOOK_HEIGHT: f32 = 130.0;

/// The camera's vertical field of view — mirrors `main::setup_camera`'s
/// `Camera3d::default()` projection (Bevy's own `PerspectiveProjection`
/// default, `fov: PI / 4.0`). [`fullscreen_pose`] derives its standoff
/// distance from this; if the app camera's FOV ever changes, this constant
/// must follow it or the fullscreen pose stops framing a panel edge-to-edge.
/// `fullscreen_pose_fills_the_frame_with_the_panel_height` locks the
/// relationship instead of trusting two hand-written copies to agree.
const CAMERA_FOV_Y: f32 = std::f32::consts::FRAC_PI_4;

// ── The shot table ───────────────────────────────────────────────────────────

/// One named camera shot the room can resolve a pose for — the "predefined,
/// tunable camera positions" table. [`resolve`] is the single entry point;
/// nothing else in the shell should synthesize a camera pose by hand.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RoomShot {
    /// Pulled-back console view, framing the whole octagon — where the camera
    /// rests for any station with no wall bearing (today only
    /// [`Station::TimeWell`]).
    RoomOverview,
    /// Room-scale pose for a wall station: standing on the SAME side of the
    /// room as its bearing, between the console and the wall, looking out at
    /// its marker/nameplate (the old `desired_camera`'s `Some` branch).
    Approach(Station),
    /// Camera fills the frame with `station`'s wall panel — "diving IS
    /// fullscreening a panel." Only valid for a station with a wall bearing;
    /// [`resolve`] panics otherwise. Today the sole zoomable station,
    /// `Station::PatchBay`, is the only one ever passed here.
    Fullscreen(Station),
    /// TODO(slice C): stub. The well has no wall bearing at all — that's the
    /// whole reason this table exists rather than reusing `Fullscreen` — so
    /// its own dolly-to-focused-ring pose needs a dedicated variant. For now
    /// resolves identically to [`RoomShot::RoomOverview`] until the real math
    /// (migrated from the well's `ease_camera_to_focused_ring`) lands.
    #[allow(dead_code)] // Slice C wires a real construction site; only tests build it today.
    WellOverview,
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
        // TODO(slice C): replace with the well's real dolly-to-focused-ring
        // pose, composed through its center placement (`placement_to_room`).
        RoomShot::WellOverview => (OVERVIEW_POS, OVERVIEW_LOOK),
    }
}

/// The **approach** pose for a wall station: standing on the SAME side of
/// the room as the focused bearing, between the console and the wall — "walk
/// toward the station you're studying" (`shell.md`'s travel-by-intent dolly).
/// Replaces the old across-the-room orbit, whose opposite-side eye put the
/// console *and* the diametrically opposite pylon in the sight line, fully
/// occluding the focused station.
///
/// Two documented per-station exceptions retarget the look point away from
/// the default marker radius/height (`super::ROOM_RADIUS`,
/// `APPROACH_LOOK_HEIGHT`):
/// - `Radiators` (2026-07-10): its NE panel is now the octagon wall shell's
///   own diagonal face, standing at [`palette::WALL_APOTHEM`] — well past
///   the old free-floating radiator radius (660) this look-point used to
///   target. Left at `super::ROOM_RADIUS` (620) the camera would look at
///   empty air short of the wall.
/// - `PatchBay` (2026-07-10, the wall-mount retune): the wheel itself moved
///   from a floor dais to the W wall panel, at [`palette::WALL_APOTHEM`] and
///   [`palette::STATION_W_MOUNT_Y`] (280, the panel's vertical center) — the
///   approach now has to rise to meet it, not look at furniture height on
///   the floor.
///
/// Every other wall station's look point is untouched. Panics if `station`
/// has no wall bearing — only [`RoomShot::focused`]/[`RoomShot::Approach`]
/// should ever reach this, and `focused` never picks `Approach` for a
/// bearing-less station.
fn approach_pose(station: Station) -> (Vec3, Vec3) {
    let d = bearing::focus_dir(station)
        .expect("RoomShot::Approach is only constructed for a station with a wall bearing");
    let (wall_r, look_h) = match station {
        Station::Radiators => (palette::WALL_APOTHEM, APPROACH_LOOK_HEIGHT),
        Station::PatchBay => (palette::WALL_APOTHEM, palette::STATION_W_MOUNT_Y),
        _ => (super::ROOM_RADIUS, APPROACH_LOOK_HEIGHT),
    };
    (
        Vec3::from_array(bearing::approach_camera(d, ROOM_CAM_APPROACH_R, ROOM_CAM_APPROACH_HEIGHT)),
        Vec3::from_array(bearing::approach_look(d, wall_r, look_h)),
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
        assert_eq!(pos.y, ROOM_CAM_APPROACH_HEIGHT);
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
    fn radiators_and_patch_bay_focus_look_at_the_wall_apothem_not_the_room_radius() {
        // Two documented exceptions read WALL_APOTHEM instead of ROOM_RADIUS:
        // Radiators (2026-07-10, the NE panel is the octagon shell's own
        // diagonal wall face, not the old free-floating slab at 660) and
        // PatchBay (2026-07-10, the wall-mount retune: the wheel itself
        // moved from a floor dais to the wall panel). Every OTHER wall
        // station's look point must be untouched.
        for s in [Station::Radiators, Station::PatchBay] {
            let (_, look) = resolve(RoomShot::focused(s));
            let d = bearing::focus_dir(s).unwrap();
            let look_r = look.x * d[0] + look.z * d[2];
            assert!(
                (look_r - palette::WALL_APOTHEM).abs() < 1e-3,
                "{s:?} should look at the wall apothem: {look_r}"
            );
        }
        for s in [Station::Tracks, Station::Vfs] {
            let (_, look) = resolve(RoomShot::focused(s));
            let d = bearing::focus_dir(s).unwrap();
            let look_r = look.x * d[0] + look.z * d[2];
            assert!(
                (look_r - super::super::ROOM_RADIUS).abs() < 1e-3,
                "{s:?} should still look at the unchanged marker radius: {look_r}"
            );
        }
    }

    #[test]
    fn patch_bay_focus_looks_at_the_mounted_wheel_height_not_furniture_height() {
        // The second half of the PatchBay exception: the look point's height
        // rises to the wall-mounted wheel's own center
        // (`palette::STATION_W_MOUNT_Y`, 280), not the floor-furniture height
        // every other wall station's look point uses (`APPROACH_LOOK_HEIGHT`).
        let (_, look) = resolve(RoomShot::focused(Station::PatchBay));
        assert!(
            (look.y - palette::STATION_W_MOUNT_Y).abs() < 1e-3,
            "PatchBay should look at the mounted wheel's own height: {}",
            look.y
        );
        let (_, tracks_look) = resolve(RoomShot::focused(Station::Tracks));
        assert_eq!(tracks_look.y, APPROACH_LOOK_HEIGHT, "other stations are unaffected");
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
    fn well_overview_stub_matches_the_room_overview_until_slice_c() {
        // TODO(slice C): once the well's real dolly-to-ring math lands, this
        // test should start FAILING — that's the signal to replace it with
        // real `WellOverview` assertions instead of loosening it.
        assert_eq!(resolve(RoomShot::WellOverview), resolve(RoomShot::RoomOverview));
    }
}
