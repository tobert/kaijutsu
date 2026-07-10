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

/// Whether `station`'s wall bearing is occupied by the station's OWN
/// instrument rather than a marker pylon + nameplate — true for
/// [`Station::PatchBay`] (`shell.md` slice B, retuned 2026-07-10: "the wheel
/// IS the west station", then wall-mounted the same day). `enter_room`'s
/// wall-placement loop reads this to skip the marker/plinth/cap/plate for a
/// furnished bearing — the wheel mounts directly on the wall panel instead,
/// no separate furniture builder needed; a future in-room station rides the
/// same gate. Pure — no Bevy types — so the gating is unit-testable without
/// spawning anything (mirrors `super::wants_gold_cap`'s shape).
pub fn station_is_room_furniture(station: Station) -> bool {
    matches!(station, Station::PatchBay)
}

// ── Octagon wall shell (`shell.md`'s cutaway centerpiece) ───────────────────
//
// Eight flat wall panels enclosing the room: faces on the four cardinals and
// the four `RADIATOR_DIRS` diagonals, `apothem` out from center. Every panel
// is a SINGLE-SIDED, inward-facing quad: `octagon_panels`' `yaw` is the
// outward angle (the `dir_theta` convention) `super`'s Bevy glue feeds into
// the same `looking_at(2×pos, Y)` trick `spawn_radiators` proved — local +Z
// (the mesh's front normal) ends up pointing at the console, so a camera
// standing beyond the panel (outside the octagon) sees its BACK — culled, by
// `StandardMaterial`'s default `cull_mode` — and the chamber shows through.
// A `Cuboid` or an explicit `cull_mode: None` on any wall part defeats this.

/// One octagon wall face. `center` is room-space, `y = 0` (the caller picks
/// the panel's vertical placement); `yaw` is the outward-facing angle the
/// face points along (`super` turns this into a `looking_at` transform, not
/// a raw rotation, so no Bevy `Quat` convention needs matching here); and
/// `bearing` names the cardinal identity hue the face's edge trim reads —
/// `None` on the four diagonals, which dress in the violet information
/// family instead.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OctagonPanel {
    pub center: [f32; 3],
    pub yaw: f32,
    pub bearing: Option<Bearing>,
}

/// The eight octagon faces, `apothem` out from center: N, NE, E, SE, S, SW,
/// W, NW in order. The diagonals are literally [`RADIATOR_DIRS`] (the same
/// four directions the floor's violet stub routes already fan toward), so a
/// re-placed diagonal can't drift out of sync between the wall and the
/// floor. Each face sits exactly 45° from its neighbours by construction.
pub fn octagon_panels(apothem: f32) -> [OctagonPanel; 8] {
    let faces: [([f32; 3], Option<Bearing>); 8] = [
        (Bearing::North.dir(), Some(Bearing::North)),
        (RADIATOR_DIRS[0], None), // NE
        (Bearing::East.dir(), Some(Bearing::East)),
        (RADIATOR_DIRS[1], None), // SE
        (Bearing::South.dir(), Some(Bearing::South)),
        (RADIATOR_DIRS[2], None), // SW
        (Bearing::West.dir(), Some(Bearing::West)),
        (RADIATOR_DIRS[3], None), // NW
    ];
    let mut out = [OctagonPanel { center: [0.0; 3], yaw: 0.0, bearing: None }; 8];
    for (i, (dir, bearing)) in faces.into_iter().enumerate() {
        out[i] = OctagonPanel {
            center: [apothem * dir[0], 0.0, apothem * dir[2]],
            yaw: dir_theta(dir),
            bearing,
        };
    }
    out
}

/// A regular octagon's flat-side width at `apothem` (the standard n-gon
/// relation, `n = 8`: side `= 2·apothem·tan(π/n)`) — the panel's FULL width,
/// touching its neighbours at the corners; `super` trims a small gap off
/// this for the corner mullions to stand in.
pub fn octagon_panel_width(apothem: f32) -> f32 {
    2.0 * apothem * core::f32::consts::FRAC_PI_8.tan()
}

/// Distance from center to an octagon vertex (where two panels meet) at
/// `apothem` — the standard n-gon relation `circumradius = apothem / cos(π/n)`.
/// Always a touch past `apothem` itself (`cos(π/8) < 1`).
pub fn octagon_circumradius(apothem: f32) -> f32 {
    apothem / core::f32::consts::FRAC_PI_8.cos()
}

/// The eight corner-mullion placements: one between every adjacent pair of
/// [`octagon_panels`] faces, sitting on the circumradius, facing outward
/// along the bisector of its two neighbours (`yaw + π/8` past the panel it
/// follows — cos/sin are periodic, so this needs no unwrapping at the
/// West→NW seam where the raw `atan2` values jump). The same inward-facing,
/// single-sided recipe applies.
pub fn octagon_corners(apothem: f32) -> [([f32; 3], f32); 8] {
    let panels = octagon_panels(apothem);
    let r = octagon_circumradius(apothem);
    let mut out = [([0.0; 3], 0.0); 8];
    for (i, p) in panels.iter().enumerate() {
        let theta = p.yaw + core::f32::consts::FRAC_PI_8;
        out[i] = ([r * theta.cos(), 0.0, r * theta.sin()], theta);
    }
    out
}

// ── Camera approach ────────────────────────────────────────────────────────────

/// Camera position for the **approach** pose: standing on the SAME side of
/// the room as the focused bearing, between the console and the wall — "walk
/// toward the station you're studying" (`shell.md`'s travel-by-intent dolly).
/// Replaces the old across-the-room orbit, whose opposite-side eye put the
/// console *and* the diametrically opposite pylon in the sight line, fully
/// occluding the focused station. `r` is the distance out from center along
/// the bearing axis; `height` the world-Y eye lift.
pub fn approach_camera(dir: [f32; 3], r: f32, height: f32) -> [f32; 3] {
    [dir[0] * r, height, dir[2] * r]
}

/// The look-at point for the approach pose: the bearing's marker at the wall
/// (`wall_r` — the same radius the marker pylons stand at), held at `look_h`
/// (nameplate height) — frames the plate square-on, straight ahead of the eye
/// on the same side of the room (never back through the console).
pub fn approach_look(dir: [f32; 3], wall_r: f32, look_h: f32) -> [f32; 3] {
    [dir[0] * wall_r, look_h, dir[2] * wall_r]
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
/// two vertices per input point offset ±half-width along the path's XZ
/// normal, each carrying a UV — `uv.x` the CUMULATIVE ARCLENGTH fraction
/// along the polyline (0 at the first point, 1 at the last), `uv.y` 0/1
/// across the ribbon's width (matching the L/R vertex order below) — plus
/// the triangle indices. Returns `(positions, uvs, indices)` — the Bevy
/// `Mesh` wrapper adds up-normals and hands it to `Assets<Mesh>`. Degenerate
/// input (< 2 points) yields empty buffers.
///
/// Arclength, not point INDEX, because a route's own sample density is wildly
/// uneven: a straight radial run is two points, a bowed arc bend is a dozen
/// (`route_points`' `arc_segments`). An index-based `t = i/(n-1)` would speed
/// a `TraceGlowMaterial` traveling-wave crest (`shaders::TraceGlowMaterial`,
/// mode 0) through the densely-sampled arc and crawl it through the sparse
/// straight runs — arclength keeps the crest's world-space speed constant
/// along the whole route regardless of how it was sampled (2026-07-10, the
/// faint-moving-glow slice).
pub fn ribbon_vertices(points: &[[f32; 3]], width: f32) -> (Vec<[f32; 3]>, Vec<[f32; 2]>, Vec<u32>) {
    if points.len() < 2 {
        return (Vec::new(), Vec::new(), Vec::new());
    }
    let half = width * 0.5;
    let n = points.len();

    // Cumulative XZ arclength up to each point, normalized to [0, 1] — the
    // room floor is XZ, +Y up (this file's module doc), so the length that
    // matters for a constant-speed crest lives entirely in XZ.
    let mut cum = vec![0.0f32; n];
    for i in 1..n {
        let dx = points[i][0] - points[i - 1][0];
        let dz = points[i][2] - points[i - 1][2];
        cum[i] = cum[i - 1] + (dx * dx + dz * dz).sqrt();
    }
    let total = cum[n - 1];

    let mut positions = Vec::with_capacity(n * 2);
    let mut uvs = Vec::with_capacity(n * 2);
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
        let u = if total > 1e-6 { cum[i] / total } else { 0.0 };
        positions.push([p[0] + nx * half, p[1], p[2] + nz * half]);
        positions.push([p[0] - nx * half, p[1], p[2] - nz * half]);
        uvs.push([u, 0.0]);
        uvs.push([u, 1.0]);
    }
    let mut indices = Vec::with_capacity((n - 1) * 6);
    for i in 0..(n - 1) {
        let a = (i * 2) as u32;
        let (l0, r0, l1, r1) = (a, a + 1, a + 2, a + 3);
        // CCW viewed from +Y, matching the injected up-normals — the old
        // `[l0, r0, l1, …]` order wound clockwise, so the geometric normal
        // faced −Y and only `unlit + cull_mode: None` hid it (issues.md
        // ribbon-winding entry, shipped 2026-07-10).
        indices.extend_from_slice(&[l0, l1, r0, r0, l1, r1]);
    }
    (positions, uvs, indices)
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

/// The world angle (radians, `arc_points`' convention: 0 = +X, +π/2 = +Z) a
/// floor-plane direction points at — ties the circuit-board route bundles to
/// the same [`Bearing::dir`] every other bearing placement reads, so a
/// re-placed bearing can't silently drift out of sync with its floor traces.
pub fn dir_theta(dir: [f32; 3]) -> f32 {
    dir[2].atan2(dir[0])
}

/// Linear interpolation, `t` typically in `[0, 1]` — shared by the board's
/// jitter ([`expand_bundle`]) and the radiator thread-strip variation
/// (`super`).
pub fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// Deterministic pseudo-random unit float from an integer seed (a
/// Murmur3-finalizer-style bit mix — no external RNG crate). The circuit
/// board must render identically every run, so its keepout invariant is
/// checkable in a unit test against the actual production seeds: the same
/// `seed` always yields the same value, in `[0, 1)`.
///
/// The mix's top 24 bits become the mantissa: `(z >> 8) / 2^24` is exact in
/// f32 and tops out at `(2^24 − 1)/2^24 < 1.0`. Dividing the full 32 bits by
/// `2^32` instead looks equivalent but is NOT — for `z` near `u32::MAX` the
/// f32 quotient rounds up to exactly 1.0, violating the half-open contract
/// (kaibo review, 2026-07-10).
pub fn hash01(seed: u32) -> f32 {
    let mut z = seed.wrapping_add(0x9E37_79B9);
    z = (z ^ (z >> 16)).wrapping_mul(0x85EB_CA6B);
    z = (z ^ (z >> 13)).wrapping_mul(0xC2B2_AE35);
    z ^= z >> 16;
    ((z >> 8) as f32) / ((1u32 << 24) as f32)
}

// ── Circuit-board floor routes (`shell.md`, "the floor is the wiring") ──────

/// One route on the circuit-board floor: a radial run from the inscribed ring
/// at angle `theta` out to a lane radius, a chamfered bend onto a concentric
/// arc that sweeps `arc_span` radians (bowing around the console — the
/// open-center rule, never crossing it), a mirrored bend, and a final radial
/// run out to a terminal pad at `pad_r`. `chamfer` is a radial length (world
/// units): both bends cut the corner this far in from the lane radius,
/// converted to an angular offset at `lane_r` so the cut stays proportionate
/// at any radius — a near-zero `arc_span` collapses the bends and the arc
/// into a single straight radial spike (the violet "short stubs"). `arc_span`'s
/// sign picks the sweep direction (the `arc_points` convention); `arc_segments`
/// is the arc's sample density. All points sit at height `y`.
pub fn route_points(
    theta: f32,
    ring_r: f32,
    lane_r: f32,
    arc_span: f32,
    pad_r: f32,
    chamfer: f32,
    arc_segments: usize,
    y: f32,
) -> Vec<[f32; 3]> {
    let dir = if arc_span >= 0.0 { 1.0 } else { -1.0 };
    let c = chamfer.min(lane_r * 0.5).max(0.0);
    let dtheta = (c / lane_r).min(arc_span.abs() * 0.25);
    let a0 = theta + dir * dtheta;
    let end_theta = theta + arc_span;
    let a1 = end_theta - dir * dtheta;

    let at = |r: f32, a: f32| [r * a.cos(), y, r * a.sin()];
    let mut pts = vec![at(ring_r, theta), at(lane_r - c, theta)];
    // A near-zero span degenerates the arc into `arc_segments + 1` identical
    // points; `ribbon_vertices` then hits its zero-length-tangent fallback and
    // snaps the ribbon's width onto the world X axis — a visible pinched twist
    // at the lane radius (both kaibo reviewers, 2026-07-10). Emit one clean
    // waypoint instead: the route reads as a straight radial stub.
    if (a1 - a0).abs() > 1e-4 {
        pts.extend(arc_points(lane_r, a0, a1, arc_segments.max(1), y));
    } else {
        pts.push(at(lane_r, theta + arc_span * 0.5));
    }
    pts.push(at(lane_r + c, end_theta));
    pts.push(at(pad_r, end_theta));
    pts
}

/// A cluster of routes fanning from the inscribed ring toward one compass
/// direction, sharing a hue family and a radius/angle band — the authoring
/// unit the rainbow board is built from (Amy, `shell.md`: "crimson toward
/// W/E, green/cyan toward N, violet short stubs toward the diagonals").
/// [`expand_bundle`] turns one bundle into `count` distinct routes.
#[derive(Debug, Clone, Copy)]
pub struct RouteBundle {
    /// Center departure angle (radians) the bundle fans around.
    pub center_theta: f32,
    /// How far the fan spreads either side of `center_theta` (radians).
    pub spread: f32,
    /// How many routes this bundle expands to.
    pub count: usize,
    /// `(min, max)` lane radius the routes' arc sweeps land at.
    pub lane_range: (f32, f32),
    /// `(min, max)` arc-span magnitude (radians); near-zero reads as a short
    /// radial stub rather than a bowed sweep.
    pub arc_range: (f32, f32),
    /// `(min, max)` terminal-pad radius.
    pub pad_range: (f32, f32),
    /// Etched hue family (linear rgb, dim — LDR; `mod.rs`'s `TRACE_*` /
    /// `RADIATOR_COLOR` constants).
    pub hue: [f32; 3],
    /// `(min, max)` brightness multiplier on `hue`.
    pub brightness_range: (f32, f32),
}

/// One generated circuit-board route: the floor-plane polyline (feed straight
/// to `ribbon_vertices`), its terminal-pad position/radius, and the etched
/// colour (hue × brightness) — everything `mod.rs` needs to spawn the ribbon
/// and its pad.
#[derive(Debug, Clone)]
pub struct BoardRoute {
    pub points: Vec<[f32; 3]>,
    pub pad_pos: [f32; 3],
    pub pad_radius: f32,
    pub hue: [f32; 3],
    pub width_scale: f32,
    pub brightness_scale: f32,
    /// Raw `hash01` draw in `[0, 1)`, this route's one slot in the faint
    /// moving-glow slice (2026-07-10): `mod.rs` derives BOTH the route's
    /// `TraceGlowMaterial` phase and its traveling-wave period from this
    /// single value (the route's 8-wide seed stride has exactly one spare
    /// slot — see [`expand_bundle`]), and the route's terminal pad reuses it
    /// too ("the same hash stream" — the pad breathes on its trace's own
    /// random draw, not a fresh one).
    pub glow_phase01: f32,
}

/// Expand a [`RouteBundle`] into its `count` routes — deterministic per
/// `(bundle, seed_base)`: route `i` draws its jitter from
/// `hash01(seed_base + i*8 + {0..=7})`, so re-running with the same inputs
/// always produces the same board (no runtime randomness — the floor is
/// procedural, not decorative). Sweep direction alternates by index so a
/// bundle fans both ways around its center, not just one way. `ring_r` (the
/// inscribed-ring departure radius), `chamfer`, `arc_segments`, `y`, and
/// `pad_disc_range` are board-wide and shared across every bundle.
pub fn expand_bundle(
    bundle: &RouteBundle,
    ring_r: f32,
    chamfer: f32,
    arc_segments: usize,
    y: f32,
    pad_disc_range: (f32, f32),
    seed_base: u32,
) -> Vec<BoardRoute> {
    (0..bundle.count)
        .map(|i| {
            let s = seed_base.wrapping_add(i as u32 * 8);
            let theta = bundle.center_theta + (hash01(s) * 2.0 - 1.0) * bundle.spread;
            let lane = lerp(bundle.lane_range.0, bundle.lane_range.1, hash01(s + 1));
            let arc_mag = lerp(bundle.arc_range.0, bundle.arc_range.1, hash01(s + 2));
            let arc_span = if i % 2 == 0 { arc_mag } else { -arc_mag };
            let pad_r = lerp(bundle.pad_range.0, bundle.pad_range.1, hash01(s + 3));
            let width_scale = lerp(0.5, 1.2, hash01(s + 4));
            let brightness_scale =
                lerp(bundle.brightness_range.0, bundle.brightness_range.1, hash01(s + 5));
            let pad_radius = lerp(pad_disc_range.0, pad_disc_range.1, hash01(s + 6));
            // The stride's one spare slot (`i*8` leaves 0..=6 taken above;
            // `s+8` would collide with route `i+1`'s own `theta` draw) — do
            // NOT shift the existing 0..=6 draws above, this must stay last.
            let glow_phase01 = hash01(s + 7);
            let points =
                route_points(theta, ring_r, lane, arc_span, pad_r, chamfer, arc_segments, y);
            let pad_pos = *points.last().expect("route_points always yields >= 2 points");
            BoardRoute {
                points,
                pad_pos,
                pad_radius,
                hue: bundle.hue,
                width_scale,
                brightness_scale,
                glow_phase01,
            }
        })
        .collect()
}

// ── Floor gradient mesh ──────────────────────────────────────────────────────

/// Flat disc mesh vertices in mesh-local XY, `[0,0,1]`-normal convention (the
/// same local space Bevy's own `Circle` mesh builds in — `mod.rs` rotates it
/// into the room's XZ floor plane, the same trick the old flat floor used): a
/// center point plus `rings` concentric rings of `segments` points, and the
/// fan/strip triangle indices between them (CCW winding, matching Bevy's
/// `Circle`/`Ellipse` mesh convention, so the front face keeps pointing +Z
/// pre-rotation). Geometry only — [`floor_color`] turns a vertex's radius
/// fraction into the radial gradient `mod.rs` bakes into
/// `Mesh::ATTRIBUTE_COLOR` (the `dome_mesh`/`dome_color` idiom, applied to the
/// floor).
pub fn disc_vertices(radius: f32, rings: usize, segments: usize) -> (Vec<[f32; 3]>, Vec<u32>) {
    let rings = rings.max(1);
    let segments = segments.max(3);
    let mut positions = vec![[0.0, 0.0, 0.0]];
    for ring in 1..=rings {
        let r = radius * ring as f32 / rings as f32;
        for seg in 0..segments {
            let a = seg as f32 / segments as f32 * core::f32::consts::TAU;
            positions.push([r * a.cos(), r * a.sin(), 0.0]);
        }
    }
    let mut indices = Vec::new();
    // Center fan → first ring.
    for seg in 0..segments {
        let a = 1 + seg;
        let b = 1 + (seg + 1) % segments;
        indices.extend_from_slice(&[0, a as u32, b as u32]);
    }
    // Quad strips between consecutive rings.
    for ring in 1..rings {
        let base0 = 1 + (ring - 1) * segments;
        let base1 = 1 + ring * segments;
        for seg in 0..segments {
            let a0 = (base0 + seg) as u32;
            let a1 = (base0 + (seg + 1) % segments) as u32;
            let b0 = (base1 + seg) as u32;
            let b1 = (base1 + (seg + 1) % segments) as u32;
            indices.extend_from_slice(&[a0, b0, b1, a0, b1, a1]);
        }
    }
    (positions, indices)
}

/// Floor radial gradient: linear-rgba vertex colour for a floor vertex whose
/// radius fraction is `t` (0 = center, under the table's glow; 1 = the rim).
/// A warm charcoal pool at center fades to the near-black rim — all LDR, the
/// floor never blooms. **Amy-tunable.**
pub fn floor_color(t: f32) -> [f32; 4] {
    const CENTER: [f32; 3] = [0.055, 0.042, 0.030];
    const RIM: [f32; 3] = [0.012, 0.015, 0.024];
    let t = t.clamp(0.0, 1.0);
    [
        CENTER[0] + (RIM[0] - CENTER[0]) * t,
        CENTER[1] + (RIM[1] - CENTER[1]) * t,
        CENTER[2] + (RIM[2] - CENTER[2]) * t,
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

    // ── station-furniture gate ──

    #[test]
    fn only_patch_bay_is_room_furniture_today() {
        for s in Station::ALL {
            assert_eq!(
                station_is_room_furniture(s),
                s == Station::PatchBay,
                "{s:?}: only the wheel stands as its own furniture today"
            );
        }
    }

    // ── octagon wall shell ──

    #[test]
    fn octagon_panels_diagonals_are_literally_the_radiator_dirs() {
        let apothem = 800.0;
        let panels = octagon_panels(apothem);
        for (slot, k) in [1usize, 3, 5, 7].iter().enumerate() {
            let expect = RADIATOR_DIRS[slot];
            assert_eq!(
                panels[*k].center,
                [apothem * expect[0], 0.0, apothem * expect[2]],
                "octagon face {k} should be RADIATOR_DIRS[{slot}]"
            );
            assert_eq!(panels[*k].bearing, None, "diagonal faces carry no cardinal identity");
        }
    }

    #[test]
    fn octagon_panels_cardinals_match_their_bearings() {
        let apothem = 800.0;
        let panels = octagon_panels(apothem);
        for (i, b) in [
            (0usize, Bearing::North),
            (2, Bearing::East),
            (4, Bearing::South),
            (6, Bearing::West),
        ] {
            let d = b.dir();
            assert_eq!(panels[i].center, [apothem * d[0], 0.0, apothem * d[2]]);
            assert_eq!(panels[i].bearing, Some(b));
        }
    }

    #[test]
    fn octagon_panels_all_sit_on_the_apothem_circle() {
        for p in octagon_panels(800.0) {
            let r = (p.center[0] * p.center[0] + p.center[2] * p.center[2]).sqrt();
            assert!((r - 800.0).abs() < 1e-3, "panel at r={r}, expected apothem 800");
        }
    }

    #[test]
    fn octagon_panels_yaw_matches_its_own_center_direction() {
        for p in octagon_panels(800.0) {
            assert!(
                (dir_theta(p.center) - p.yaw).abs() < 1e-5,
                "yaw should be the outward angle of its own position: {p:?}"
            );
        }
    }

    #[test]
    fn octagon_panels_are_evenly_spaced_45_degrees_apart() {
        let panels = octagon_panels(800.0);
        for i in 0..8 {
            let a = panels[i].yaw;
            let b = panels[(i + 1) % 8].yaw;
            // Unwrap the one seam where the raw atan2 values jump backwards
            // (West, θ=π → NW, θ=−3π/4): every other neighbour pair already
            // steps forward by +π/4.
            let mut step = b - a;
            if step < 0.0 {
                step += core::f32::consts::TAU;
            }
            assert!(
                (step - core::f32::consts::FRAC_PI_4).abs() < 1e-4,
                "panels {i}->{} should be 45 degrees apart, got {step}",
                (i + 1) % 8
            );
        }
    }

    #[test]
    fn octagon_panel_width_matches_the_regular_octagon_side_formula() {
        let apothem = 800.0;
        let expected = 2.0 * apothem * (std::f32::consts::PI / 8.0).tan();
        assert!((octagon_panel_width(apothem) - expected).abs() < 1e-3);
        assert!(octagon_panel_width(apothem) > 0.0);
    }

    #[test]
    fn octagon_circumradius_is_a_touch_past_the_apothem() {
        let apothem = 800.0;
        let r = octagon_circumradius(apothem);
        assert!(r > apothem, "the vertex sits farther out than the flat face: {r}");
        assert!((r - apothem / (std::f32::consts::PI / 8.0).cos()).abs() < 1e-3);
    }

    #[test]
    fn octagon_corners_sit_on_the_circumradius_bisecting_their_two_panels() {
        let apothem = 800.0;
        let panels = octagon_panels(apothem);
        let circumradius = octagon_circumradius(apothem);
        for (i, (pos, theta)) in octagon_corners(apothem).into_iter().enumerate() {
            let r = (pos[0] * pos[0] + pos[2] * pos[2]).sqrt();
            assert!((r - circumradius).abs() < 1e-3, "corner {i} at r={r}");
            assert!(
                (theta - (panels[i].yaw + core::f32::consts::FRAC_PI_8)).abs() < 1e-5,
                "corner {i} bisects panel {i} and its next neighbour"
            );
        }
    }

    // ── camera approach ──

    #[test]
    fn facing_east_puts_the_camera_on_the_east_side_approaching_the_wall() {
        let d = Bearing::East.dir();
        let cam = approach_camera(d, 160.0, 190.0);
        assert!(cam[0] > 0.0, "camera stands on the same (east) side: {cam:?}");
        assert_eq!(cam[1], 190.0, "lifted to the given eye height");
        let look = approach_look(d, 620.0, 150.0);
        assert!(look[0] > cam[0], "looks further east, out toward the wall: {look:?}");
    }

    #[test]
    fn facing_west_mirrors_facing_east() {
        let cam_e = approach_camera(Bearing::East.dir(), 160.0, 190.0);
        let cam_w = approach_camera(Bearing::West.dir(), 160.0, 190.0);
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
        let (pos, uvs, idx) = ribbon_vertices(&line, 10.0);
        assert_eq!(pos.len(), 6, "2 verts × 3 points");
        assert_eq!(uvs.len(), 6, "one uv per vertex");
        assert_eq!(idx.len(), 12, "6 indices × 2 segments");
        // Straight line along +X → the ±width offset is along ±Z.
        assert!((pos[0][2] - 5.0).abs() < 1e-4 || (pos[0][2] + 5.0).abs() < 1e-4);
        assert!((pos[0][0]).abs() < 1e-4, "no offset along the tangent");
    }

    #[test]
    fn ribbon_vertices_reject_degenerate_input() {
        let (pos, uvs, idx) = ribbon_vertices(&[[0.0, 0.0, 0.0]], 10.0);
        assert!(pos.is_empty() && uvs.is_empty() && idx.is_empty());
    }

    #[test]
    fn ribbon_vertices_uv_x_is_monotonic_and_spans_0_to_1() {
        let line = [[0.0, 0.0, 0.0], [30.0, 0.0, 0.0], [100.0, 0.0, 0.0], [100.0, 0.0, 50.0]];
        let (_, uvs, _) = ribbon_vertices(&line, 10.0);
        let u_at = |i: usize| uvs[i * 2][0];
        assert!(u_at(0).abs() < 1e-6, "starts at 0: {}", u_at(0));
        assert!((u_at(line.len() - 1) - 1.0).abs() < 1e-6, "ends at 1: {}", u_at(line.len() - 1));
        for i in 0..line.len() - 1 {
            assert!(u_at(i) <= u_at(i + 1) + 1e-6, "uv.x must be monotonic along the polyline");
        }
    }

    #[test]
    fn ribbon_vertices_uv_x_tracks_arclength_not_point_index() {
        // Three points, very unevenly spaced: the first hop is 10x the
        // second. An index-based `t` would put the middle point at 0.5;
        // arclength puts it at 100/110 instead — the whole point of tying
        // the traveling-wave crest's speed to real distance, not sample
        // density (`ribbon_vertices`'s own doc).
        let line = [[0.0, 0.0, 0.0], [100.0, 0.0, 0.0], [110.0, 0.0, 0.0]];
        let (_, uvs, _) = ribbon_vertices(&line, 10.0);
        let mid_u = uvs[2][0];
        assert!(
            (mid_u - 100.0 / 110.0).abs() < 1e-4,
            "uv.x should reflect distance traveled, not index position: {mid_u}"
        );
    }

    #[test]
    fn ribbon_vertices_uv_y_is_0_and_1_across_the_l_r_pair() {
        let line = [[0.0, 0.0, 0.0], [50.0, 0.0, 0.0]];
        let (_, uvs, _) = ribbon_vertices(&line, 10.0);
        for pair in uvs.chunks(2) {
            assert_eq!(pair[0][1], 0.0, "L side at v=0");
            assert_eq!(pair[1][1], 1.0, "R side at v=1");
            assert_eq!(pair[0][0], pair[1][0], "L/R share the same uv.x");
        }
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

    // ── dir_theta / lerp / hash01 ──

    #[test]
    fn dir_theta_matches_the_cardinal_bearings() {
        assert!(dir_theta(Bearing::East.dir()).abs() < 1e-6);
        assert!(
            (dir_theta(Bearing::North.dir()) - (-core::f32::consts::FRAC_PI_2)).abs() < 1e-6
        );
        assert!((dir_theta(Bearing::South.dir()) - core::f32::consts::FRAC_PI_2).abs() < 1e-6);
        assert!((dir_theta(Bearing::West.dir()).abs() - core::f32::consts::PI).abs() < 1e-6);
    }

    #[test]
    fn lerp_interpolates_between_endpoints() {
        assert_eq!(lerp(10.0, 20.0, 0.0), 10.0);
        assert_eq!(lerp(10.0, 20.0, 1.0), 20.0);
        assert_eq!(lerp(10.0, 20.0, 0.5), 15.0);
    }

    #[test]
    fn hash01_is_deterministic_and_stays_in_unit_range() {
        for seed in [0u32, 1, 42, 1000, u32::MAX] {
            let a = hash01(seed);
            let b = hash01(seed);
            assert_eq!(a, b, "same seed, same value");
            assert!((0.0..1.0).contains(&a), "hash01({seed}) = {a} out of [0,1)");
        }
    }

    #[test]
    fn hash01_varies_across_seeds() {
        let vals: Vec<f32> = (0..8).map(hash01).collect();
        let all_same = vals.windows(2).all(|w| w[0] == w[1]);
        assert!(!all_same, "distinct seeds should not all collide: {vals:?}");
    }

    // ── route_points ──

    #[test]
    fn route_points_starts_at_the_ring_and_ends_at_the_pad() {
        let pts = route_points(0.0, 230.0, 400.0, 0.6, 700.0, 18.0, 12, 0.6);
        let first = pts.first().unwrap();
        let last = pts.last().unwrap();
        let r = |p: &[f32; 3]| (p[0] * p[0] + p[2] * p[2]).sqrt();
        assert!((r(first) - 230.0).abs() < 1e-3, "starts on the ring: {first:?}");
        assert!((r(last) - 700.0).abs() < 1e-3, "ends on the pad: {last:?}");
        assert!(pts.iter().all(|p| p[1] == 0.6), "every point at the given height");
    }

    #[test]
    fn route_points_stays_outside_the_keepout_when_ring_and_lane_clear_it() {
        const KEEPOUT: f32 = 150.0;
        for arc_span in [-0.8, -0.1, 0.0, 0.1, 0.8] {
            let pts = route_points(0.4, 230.0, 350.0, arc_span, 600.0, 18.0, 10, 0.6);
            for p in &pts {
                let r = (p[0] * p[0] + p[2] * p[2]).sqrt();
                assert!(
                    r > KEEPOUT,
                    "route point at r={r} crosses the console keep-out (arc_span={arc_span})"
                );
            }
        }
    }

    #[test]
    fn route_points_positive_and_negative_arc_span_sweep_opposite_ways() {
        let pos = route_points(0.0, 230.0, 400.0, 0.8, 700.0, 18.0, 8, 0.0);
        let neg = route_points(0.0, 230.0, 400.0, -0.8, 700.0, 18.0, 8, 0.0);
        assert!(pos.last().unwrap()[2] > 0.0, "positive arc_span sweeps toward +Z");
        assert!(neg.last().unwrap()[2] < 0.0, "negative arc_span sweeps toward -Z");
    }

    // ── RouteBundle / expand_bundle ──

    fn sample_bundle() -> RouteBundle {
        RouteBundle {
            center_theta: 0.7,
            spread: 0.5,
            count: 9,
            lane_range: (240.0, 700.0),
            arc_range: (0.0, 0.9),
            pad_range: (300.0, 950.0),
            hue: [0.3, 0.05, 0.1],
            brightness_range: (0.7, 1.2),
        }
    }

    #[test]
    fn expand_bundle_yields_the_requested_count() {
        let routes = expand_bundle(&sample_bundle(), 230.0, 18.0, 10, 0.6, (6.0, 14.0), 1000);
        assert_eq!(routes.len(), 9);
    }

    #[test]
    fn expand_bundle_is_deterministic() {
        let a = expand_bundle(&sample_bundle(), 230.0, 18.0, 10, 0.6, (6.0, 14.0), 42);
        let b = expand_bundle(&sample_bundle(), 230.0, 18.0, 10, 0.6, (6.0, 14.0), 42);
        assert_eq!(a.len(), b.len());
        for (ra, rb) in a.iter().zip(b.iter()) {
            assert_eq!(ra.points, rb.points, "same seed, same board");
            assert_eq!(ra.pad_radius, rb.pad_radius);
            assert_eq!(ra.brightness_scale, rb.brightness_scale);
            assert_eq!(ra.glow_phase01, rb.glow_phase01, "same seed, same glow phase");
        }
    }

    #[test]
    fn expand_bundle_glow_phase01_stays_in_unit_range_and_varies_across_routes() {
        let routes = expand_bundle(&sample_bundle(), 230.0, 18.0, 10, 0.6, (6.0, 14.0), 123);
        for r in &routes {
            assert!(
                (0.0..1.0).contains(&r.glow_phase01),
                "glow_phase01 out of range: {}",
                r.glow_phase01
            );
        }
        let all_same = routes.windows(2).all(|w| w[0].glow_phase01 == w[1].glow_phase01);
        assert!(!all_same, "distinct routes should get distinct glow phases: {routes:?}");
    }

    #[test]
    fn expand_bundle_pad_radius_stays_in_the_requested_disc_range() {
        for r in expand_bundle(&sample_bundle(), 230.0, 18.0, 10, 0.6, (6.0, 14.0), 7) {
            assert!(
                (6.0..=14.0).contains(&r.pad_radius),
                "pad radius {} out of range",
                r.pad_radius
            );
        }
    }

    #[test]
    fn expand_bundle_every_generated_route_clears_the_keepout() {
        // The concrete invariant the whole board leans on: no matter how the
        // per-route jitter lands, every point of every generated route stays
        // outside the console keep-out (`shell.md`'s open-center rule).
        const KEEPOUT: f32 = 150.0;
        for route in expand_bundle(&sample_bundle(), 230.0, 18.0, 10, 0.6, (6.0, 14.0), 99) {
            for p in &route.points {
                let r = (p[0] * p[0] + p[2] * p[2]).sqrt();
                assert!(r > KEEPOUT, "route point at r={r} crosses the console keep-out");
            }
        }
    }

    // ── disc_vertices / floor_color ──

    #[test]
    fn disc_vertices_counts_match_rings_and_segments() {
        let (pos, idx) = disc_vertices(500.0, 6, 24);
        assert_eq!(pos.len(), 1 + 6 * 24, "center + rings*segments");
        assert_eq!(idx.len(), 3 * 24 * (2 * 6 - 1), "fan + strip triangles");
        assert!(idx.iter().all(|&i| (i as usize) < pos.len()), "every index in range");
    }

    #[test]
    fn disc_vertices_outer_ring_sits_on_the_radius() {
        let (pos, _) = disc_vertices(500.0, 4, 16);
        for p in pos.iter().skip(1 + 3 * 16) {
            let r = (p[0] * p[0] + p[1] * p[1]).sqrt();
            assert!((r - 500.0).abs() < 1e-2, "outer ring point at r={r}");
        }
    }

    #[test]
    fn disc_vertices_rejects_degenerate_inputs_by_flooring_them() {
        let (pos, idx) = disc_vertices(100.0, 0, 1);
        assert_eq!(pos.len(), 1 + 3, "rings floored to 1, segments floored to 3");
        assert!(!idx.is_empty());
    }

    #[test]
    fn floor_color_pools_warm_at_center_and_fades_to_near_black_at_the_rim() {
        let center = floor_color(0.0);
        let rim = floor_color(1.0);
        let lum = |c: [f32; 4]| c[0] + c[1] + c[2];
        assert!(lum(center) > lum(rim), "center pools brighter than the rim");
        assert!(lum(center) < 1.0, "never blooms — the floor stays calm LDR");
        assert_eq!(center[3], 1.0, "opaque");
    }
}
