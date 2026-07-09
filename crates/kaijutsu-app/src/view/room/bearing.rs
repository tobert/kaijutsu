//! Pure room geometry (`docs/scenes/shell.md`, "Geometry and bearings"): the
//! compass **bearings** stations sit at, the carousel→bearing mapping, the
//! camera-orbit math that faces a bearing, and the procedural-mesh vertex
//! helpers for the floor traces and the vault gradient.
//!
//! No Bevy types — `[f32; 3]` points (x, y, z; the room floor is XZ, +Y up)
//! and plain `f32`, unit-tested, same stance as `view/patch_bay/geometry.rs`.
//! The Bevy glue in [`super`] turns these arrays into `Vec3`/`Transform`s and
//! `Mesh`es.

use super::nav::Station;

// ── Bearings ────────────────────────────────────────────────────────────────

/// A stable compass placement in the circular room. Cardinals sit on the wall;
/// `Center` is the console (the time well). The floor is XZ with +Y up, so a
/// bearing's [`dir`](Bearing::dir) is a unit XZ vector (y = 0):
/// North = −Z (into the room), South = +Z (toward the entering camera),
/// East = +X, West = −X. `Center` has no direction (`[0,0,0]`).
///
/// These are **hand-assigned and stable** — a bearing you learn is an address
/// (the track-rays lesson, `docs/scenes/README.md`). Settling of open question
/// 1 in `shell.md`: TimeWell = center, PatchBay = W, Tracks = E, VFS = N,
/// reserved = S.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Bearing {
    Center,
    North,
    East,
    South,
    West,
}

impl Bearing {
    /// How many bearings — the width of the per-bearing activity array
    /// ([`super::activity::BearingActivity`]).
    pub const COUNT: usize = 5;

    /// Dense index for array keying (`Center` = 0 … `West` = 4).
    pub fn index(self) -> usize {
        match self {
            Bearing::Center => 0,
            Bearing::North => 1,
            Bearing::East => 2,
            Bearing::South => 3,
            Bearing::West => 4,
        }
    }

    /// Unit XZ direction from the room center toward this bearing's wall
    /// (`[0,0,0]` for `Center`). +Y is 0 — bearings are floor directions.
    pub fn dir(self) -> [f32; 3] {
        match self {
            Bearing::Center => [0.0, 0.0, 0.0],
            Bearing::North => [0.0, 0.0, -1.0],
            Bearing::East => [1.0, 0.0, 0.0],
            Bearing::South => [0.0, 0.0, 1.0],
            Bearing::West => [-1.0, 0.0, 0.0],
        }
    }
}

/// Normalized NE diagonal — where the `Radiators` carousel entry points the
/// camera (a between-station wall panel, not a cardinal station). `√½`.
pub const RADIATOR_FOCUS_DIR: [f32; 3] = [core::f32::consts::FRAC_1_SQRT_2, 0.0, -core::f32::consts::FRAC_1_SQRT_2];

/// The four between-station wall bearings (the diagonals) where the violet
/// information radiators stand: NE, SE, SW, NW.
pub const RADIATOR_DIRS: [[f32; 3]; 4] = [
    [core::f32::consts::FRAC_1_SQRT_2, 0.0, -core::f32::consts::FRAC_1_SQRT_2], // NE
    [core::f32::consts::FRAC_1_SQRT_2, 0.0, core::f32::consts::FRAC_1_SQRT_2],  // SE
    [-core::f32::consts::FRAC_1_SQRT_2, 0.0, core::f32::consts::FRAC_1_SQRT_2], // SW
    [-core::f32::consts::FRAC_1_SQRT_2, 0.0, -core::f32::consts::FRAC_1_SQRT_2], // NW
];

/// The direction the camera should face when a station is focused, or `None`
/// for the console (`TimeWell`), which frames the room from the overview pose.
/// `Radiators` faces a between-station wall panel ([`RADIATOR_FOCUS_DIR`]).
/// This is the carousel→bearing mapping the camera dolly reads.
pub fn focus_dir(station: Station) -> Option<[f32; 3]> {
    match station {
        Station::TimeWell => None,
        Station::PatchBay => Some(Bearing::West.dir()),
        Station::Tracks => Some(Bearing::East.dir()),
        Station::Vfs => Some(Bearing::North.dir()),
        Station::Radiators => Some(RADIATOR_FOCUS_DIR),
    }
}

// ── Wall placements ───────────────────────────────────────────────────────────

/// One occupied wall bearing: which station stands there (if any — the reserved
/// South marker has no station), its facing direction, and its marker hue
/// (linear rgb, the LDR identity colour lifted to HDR only on live activity).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WallPlacement {
    pub station: Option<Station>,
    pub bearing: Bearing,
    pub dir: [f32; 3],
    /// Linear-rgb identity hue for the marker pylon.
    pub hue: [f32; 3],
}

/// The four cardinal wall placements, in `Station::ALL`-adjacent order (W, E,
/// N, S). The reserved South bearing renders a dim, unlabeled marker (no
/// station, grey hue) — the placeholder for the future MCP/LLM engine room
/// (`shell.md`). Hues avoid the reserved fabric hues except where a station
/// owns one later: patch bay leans crimson (MIDI), tracks amber (rhythm), VFS
/// green (data horizon). **Amy-tunable.**
pub fn wall_placements() -> [WallPlacement; 4] {
    [
        WallPlacement {
            station: Some(Station::PatchBay),
            bearing: Bearing::West,
            dir: Bearing::West.dir(),
            hue: [0.90, 0.28, 0.34],
        },
        WallPlacement {
            station: Some(Station::Tracks),
            bearing: Bearing::East,
            dir: Bearing::East.dir(),
            hue: [1.00, 0.60, 0.22],
        },
        WallPlacement {
            station: Some(Station::Vfs),
            bearing: Bearing::North,
            dir: Bearing::North.dir(),
            hue: [0.40, 0.85, 0.52],
        },
        WallPlacement {
            station: None,
            bearing: Bearing::South,
            dir: Bearing::South.dir(),
            hue: [0.42, 0.44, 0.52],
        },
    ]
}

// ── Camera orbit ──────────────────────────────────────────────────────────────

/// Camera position that faces `dir`'s wall from the **opposite** side of the
/// room, elevated, so the console anchors the foreground and the focused
/// station rises beyond it. `orbit` is the back-off distance along the bearing
/// axis; `height` the world-Y lift.
pub fn orbit_camera(dir: [f32; 3], orbit: f32, height: f32) -> [f32; 3] {
    [-dir[0] * orbit, height, -dir[2] * orbit]
}

/// The look-at point when facing `dir`: a touch past the console toward the
/// station, at `look_h` height — frames console + station together.
pub fn orbit_look(dir: [f32; 3], lead: f32, look_h: f32) -> [f32; 3] {
    [dir[0] * lead, look_h, dir[2] * lead]
}

// ── Procedural-mesh vertex helpers ─────────────────────────────────────────────

/// Sample a circular arc in the room floor (XZ) at `radius`, from angle `a0`
/// to `a1` (radians, standard math convention: 0 = +X, +π/2 = +Z), `segments`
/// spans → `segments + 1` points, all at height `y`. Concentric with the room
/// center by construction, so an arc trace **bows around the center and never
/// passes under it** (the open-center rule, `shell.md` "the floor is the
/// wiring").
pub fn arc_points(radius: f32, a0: f32, a1: f32, segments: usize, y: f32) -> Vec<[f32; 3]> {
    let n = segments.max(1);
    (0..=n)
        .map(|i| {
            let t = i as f32 / n as f32;
            let a = a0 + (a1 - a0) * t;
            [radius * a.cos(), y, radius * a.sin()]
        })
        .collect()
}

/// Expand a floor polyline into a flat ribbon of `width` (in the XZ plane):
/// two vertices per input point offset ±half-width along the path's XZ normal,
/// plus the triangle indices. Returns `(positions, indices)` — the Bevy `Mesh`
/// wrapper adds up-normals and hands it to `Assets<Mesh>`. Degenerate input
/// (< 2 points) yields empty buffers.
pub fn ribbon_vertices(points: &[[f32; 3]], width: f32) -> (Vec<[f32; 3]>, Vec<u32>) {
    if points.len() < 2 {
        return (Vec::new(), Vec::new());
    }
    let half = width * 0.5;
    let n = points.len();
    let mut positions = Vec::with_capacity(n * 2);
    for i in 0..n {
        // Tangent via central difference (forward/backward at the ends).
        let prev = points[i.saturating_sub(1)];
        let next = points[(i + 1).min(n - 1)];
        let tx = next[0] - prev[0];
        let tz = next[2] - prev[2];
        let len = (tx * tx + tz * tz).sqrt();
        let (tx, tz) = if len > 1e-6 { (tx / len, tz / len) } else { (1.0, 0.0) };
        // 90° rotation in XZ → the ribbon's half-width normal.
        let (nx, nz) = (-tz, tx);
        let p = points[i];
        positions.push([p[0] + nx * half, p[1], p[2] + nz * half]);
        positions.push([p[0] - nx * half, p[1], p[2] - nz * half]);
    }
    let mut indices = Vec::with_capacity((n - 1) * 6);
    for i in 0..(n - 1) {
        let a = (i * 2) as u32;
        let (l0, r0, l1, r1) = (a, a + 1, a + 2, a + 3);
        indices.extend_from_slice(&[l0, r0, l1, r0, r1, l1]);
    }
    (positions, indices)
}

/// Vault gradient: linear-rgba vertex colour for a dome vertex whose height
/// fraction is `t` (0 = horizon rim, 1 = apex overhead). Apex is the darkest
/// (calm darkness overhead); the rim lifts to a faint cool violet so the vault
/// reads as an enclosing surface rather than void. All LDR — the dome never
/// blooms (`shell.md` open question 4: no starfield, no firmament yet).
/// **Amy-tunable.**
pub fn dome_color(t: f32) -> [f32; 4] {
    const RIM: [f32; 3] = [0.050, 0.048, 0.086];
    const APEX: [f32; 3] = [0.013, 0.017, 0.038];
    let t = t.clamp(0.0, 1.0);
    [
        RIM[0] + (APEX[0] - RIM[0]) * t,
        RIM[1] + (APEX[1] - RIM[1]) * t,
        RIM[2] + (APEX[2] - RIM[2]) * t,
        1.0,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Bearings ──

    #[test]
    fn cardinal_directions_are_unit_and_axis_aligned() {
        assert_eq!(Bearing::North.dir(), [0.0, 0.0, -1.0]);
        assert_eq!(Bearing::South.dir(), [0.0, 0.0, 1.0]);
        assert_eq!(Bearing::East.dir(), [1.0, 0.0, 0.0]);
        assert_eq!(Bearing::West.dir(), [-1.0, 0.0, 0.0]);
        assert_eq!(Bearing::Center.dir(), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn opposite_bearings_are_negatives() {
        let n = Bearing::North.dir();
        let s = Bearing::South.dir();
        assert_eq!([n[0], n[1], n[2]], [-s[0], -s[1], -s[2]]);
        let e = Bearing::East.dir();
        let w = Bearing::West.dir();
        assert_eq!([e[0], e[1], e[2]], [-w[0], -w[1], -w[2]]);
    }

    #[test]
    fn bearing_indices_are_dense_and_distinct() {
        let all = [
            Bearing::Center,
            Bearing::North,
            Bearing::East,
            Bearing::South,
            Bearing::West,
        ];
        let mut seen = [false; Bearing::COUNT];
        for b in all {
            assert!(b.index() < Bearing::COUNT);
            assert!(!seen[b.index()], "index reused");
            seen[b.index()] = true;
        }
        assert!(seen.iter().all(|&s| s), "every slot used");
    }

    #[test]
    fn radiator_focus_dir_is_a_unit_ne_diagonal() {
        let d = RADIATOR_FOCUS_DIR;
        let len = (d[0] * d[0] + d[2] * d[2]).sqrt();
        assert!((len - 1.0).abs() < 1e-6, "unit length: {len}");
        assert!(d[0] > 0.0 && d[2] < 0.0, "points north-east (+X, −Z)");
    }

    // ── carousel → bearing mapping ──

    #[test]
    fn console_focus_is_the_overview_no_direction() {
        assert_eq!(focus_dir(Station::TimeWell), None);
    }

    #[test]
    fn stations_map_to_their_stable_bearings() {
        assert_eq!(focus_dir(Station::PatchBay), Some(Bearing::West.dir()));
        assert_eq!(focus_dir(Station::Tracks), Some(Bearing::East.dir()));
        assert_eq!(focus_dir(Station::Vfs), Some(Bearing::North.dir()));
        assert_eq!(focus_dir(Station::Radiators), Some(RADIATOR_FOCUS_DIR));
    }

    #[test]
    fn tracks_bearing_is_east_where_the_beat_glow_lands() {
        // The acceptance signal (a jam breathes the tracks marker) is keyed to
        // the East bearing; guard the mapping so a re-order can't silently move
        // the glow off the tracks station.
        let placements = wall_placements();
        let tracks = placements
            .iter()
            .find(|p| p.station == Some(Station::Tracks))
            .expect("tracks has a wall placement");
        assert_eq!(tracks.bearing, Bearing::East);
    }

    #[test]
    fn reserved_south_marker_has_no_station() {
        let placements = wall_placements();
        let south = placements.iter().find(|p| p.bearing == Bearing::South).unwrap();
        assert_eq!(south.station, None, "South is reserved — a dim marker, no label");
    }

    // ── camera orbit ──

    #[test]
    fn facing_east_puts_the_camera_on_the_west_side_looking_east() {
        let d = Bearing::East.dir();
        let cam = orbit_camera(d, 700.0, 250.0);
        assert!(cam[0] < 0.0, "camera sits west of center: {cam:?}");
        assert_eq!(cam[1], 250.0, "lifted to the given height");
        let look = orbit_look(d, 180.0, 120.0);
        assert!(look[0] > 0.0, "looks toward the east station: {look:?}");
    }

    #[test]
    fn facing_west_mirrors_facing_east() {
        let cam_e = orbit_camera(Bearing::East.dir(), 700.0, 250.0);
        let cam_w = orbit_camera(Bearing::West.dir(), 700.0, 250.0);
        assert_eq!(cam_e[0], -cam_w[0], "mirror across the room");
        assert_eq!(cam_e[2], cam_w[2]);
    }

    // ── arc / ribbon / dome ──

    #[test]
    fn arc_points_stay_on_their_radius_never_reaching_the_center() {
        let pts = arc_points(300.0, 0.0, std::f32::consts::PI, 12, 0.5);
        assert_eq!(pts.len(), 13, "segments + 1 points");
        for p in &pts {
            let r = (p[0] * p[0] + p[2] * p[2]).sqrt();
            assert!((r - 300.0).abs() < 1e-3, "on the arc radius: {r}");
            assert_eq!(p[1], 0.5, "held at the trace height");
        }
    }

    #[test]
    fn ribbon_vertices_are_two_per_point_and_six_indices_per_segment() {
        let line = [[0.0, 0.0, 0.0], [100.0, 0.0, 0.0], [200.0, 0.0, 0.0]];
        let (pos, idx) = ribbon_vertices(&line, 10.0);
        assert_eq!(pos.len(), 6, "2 verts × 3 points");
        assert_eq!(idx.len(), 12, "6 indices × 2 segments");
        // Straight line along +X → the ±width offset is along ±Z.
        assert!((pos[0][2] - 5.0).abs() < 1e-4 || (pos[0][2] + 5.0).abs() < 1e-4);
        assert!((pos[0][0]).abs() < 1e-4, "no offset along the tangent");
    }

    #[test]
    fn ribbon_vertices_reject_degenerate_input() {
        let (pos, idx) = ribbon_vertices(&[[0.0, 0.0, 0.0]], 10.0);
        assert!(pos.is_empty() && idx.is_empty());
    }

    #[test]
    fn dome_gradient_darkens_toward_the_apex_and_stays_ldr() {
        let rim = dome_color(0.0);
        let apex = dome_color(1.0);
        let lum = |c: [f32; 4]| c[0] + c[1] + c[2];
        assert!(lum(apex) < lum(rim), "apex overhead is the darkest");
        assert!(lum(rim) < 1.0, "never blooms — the vault stays calm LDR");
        assert_eq!(rim[3], 1.0, "opaque");
    }
}
