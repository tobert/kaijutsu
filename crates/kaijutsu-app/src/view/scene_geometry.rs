//! Shared cross-scene **geometry** contracts for the kernel interior
//! (`docs/scenes/`) — the room's octagon shell and the W-wall patch-wheel
//! mount, the datums `room/mod.rs` and `patch_bay/mod.rs` both have to agree
//! on without eyeballing each other.
//!
//! Color no longer lives here (`docs/color.md`'s scene-lane contract): every
//! scene module's identity hues, brightness tiers, and live-signal gains
//! read from `Res<`[`crate::view::scene_palette::ScenePalette`]`>` — the
//! app-side face of `[scene]` in `theme.toml`. This module's old color/
//! brightness constants (`GOLD_HUE`, `BRASS_HUE`, `VIOLET_GLASS`,
//! `VIOLET_THREAD`, `GLOW_CREST`, `GLOW_TROUGH_SUBTLE`, …) moved onto that
//! resource's fields (`gold`, `brass`, `violet_glass`, `violet_thread`,
//! `crest`, `trough_subtle`, …) — see `ScenePalette::default()` for the
//! compiled mirror of every value that used to live here as a flat const.
//! Scene modules must not define private color/brightness constants any
//! more — new color goes through `ScenePalette`.
//!
//! **Material discipline** (the room's rule, scene-family-wide): every
//! material is built-in `StandardMaterial` with `unlit: true`, brightness
//! carried in `base_color` — LDR (< 1.0 linear) reads as calm etched
//! structure, HDR (> 1.0) blooms through the app camera's threshold-1.0
//! bloom and is reserved for **live activity**. No point lights, no lit
//! metals: a ~1%-albedo metallic surface swallows any lamp —
//! emissive-on-dark is the concepts' look anyway. Decoration may ALSO carry a
//! faint, slowly moving glow on top of that discipline — a traveling-wave
//! crest or a slow uniform breath, rendered by
//! [`crate::shaders::TraceGlowMaterial`] instead of `StandardMaterial`; see
//! `ScenePalette`'s `crest`/`trough_*` fields for the tier ladder this rides.
//!
//! Hues live on `ScenePalette` as linear [`bevy::prelude::LinearRgba`];
//! multiply by a tier or gain before handing them to a material
//! (`scene_palette::lin_scaled` et al.).

// ── Station W contract (the patch wheel mounted ON the west wall) ──────────
// Amy, 2026-07-10: "mount the wheel ON the W wall panel — part of the wall,
// not furniture in front of it" ("the surface gets taken over by its
// content"; studio patch bays are wall panels; concept 06 draws the W
// station wall-mounted with threads dropping into the floor traces). This
// supersedes the dais contract (`STATION_W_X`/`_DAIS_TOP_Y`/`_DAIS_R`, all
// deleted here, along with `room::spawn_w_dais`, the furniture builder that
// read them): the wheel no longer stands on furniture at W, it hangs on the
// wall itself, re-oriented face-out by `patch_bay::STATION_W_PLACEMENT`'s
// pitch+yaw (`patch_bay.rs`'s doc on that constant has the quaternion
// derivation). These constants remain the one shared agreement between the
// two files — `room::spawn_walls` builds the W panel, reading `WALL_APOTHEM`
// from here; `patch_bay`'s placement reads all four to seat the wheel flush
// against that same panel — so neither file can drift without the other
// noticing here.

/// Octagon wall apothem (center-to-face distance) — moved here from
/// `room/mod.rs` (2026-07-10, the wall-mount slice) now that it is a
/// cross-file architectural datum: `room::spawn_walls` still builds the
/// panel geometry (untouched) at this radius, and `patch_bay`'s placement now
/// reads the SAME number to seat the wheel flush against the W panel it
/// stands on.
///
/// Bumped 800 → 1200 (2026-07-10 evening, the fullscreen-panel pivot — Amy:
/// "the walls are 16:9 screens, and diving IS fullscreening a panel"): at
/// 1200 a panel's FULL width (`bearing::octagon_panel_width`) is
/// `2·1200·tan(π/8) ≈ 994`, against the unchanged `room::WALL_HEIGHT` (560) —
/// a 994:560 ≈ 16:9 frame, so `room::fullscreen_pose` fills the camera's
/// vertical frustum with exactly one panel, edge to edge. Clears the old
/// radiator radius (660) and the wall-station radius the pylons/markers
/// stand at (`room::ROOM_RADIUS`, 620) same as before; the new binding
/// constraint is the octagon's own circumradius
/// (`bearing::octagon_circumradius`, ≈1299 at this apothem), which must stay
/// under `room::FLOOR_RADIUS` (1300) so the walls stand ON the floor disc,
/// not past its edge.
pub(crate) const WALL_APOTHEM: f32 = 1200.0;

/// Wheel-center height on the W panel — the panel's own vertical center
/// (`room::WALL_HEIGHT` 560 / 2 = 280): the mounted face reads centered on
/// its wall, the way a studio patch bay hangs mid-height, not crowded to the
/// floor or the ceiling.
pub(crate) const STATION_W_MOUNT_Y: f32 = 280.0;

/// How far the wheel's tabletop plane (the placement's local origin — local
/// +Y is the wheel's table normal) floats proud of the W panel face, along
/// world +X (into the room). **Amy-tunable — the flip that decides the
/// station's whole read**: near-zero means **inset-flush**, "part of the
/// wall" rather than furniture standing in front of it (the 2026-07-10
/// mount-on-the-wall call); a larger value lets the instrument stand a
/// little proud, like a shallow wall-mounted console.
///
/// Either way, no coplanar-seating lift is needed the way the old dais
/// required one (`translation.y` used to add `TABLE_DEPTH * scale` so the
/// table's underside landed exactly on the dais's real, load-bearing top
/// face — or z-fight/float otherwise). Here the table's solid thickness
/// extrudes the OTHER direction: local −Y → world −X (`patch_bay.rs`'s
/// `STATION_W_PLACEMENT` doc has the rotation), toward and through the panel
/// — not onto a surface it has to land on exactly. The panel is a
/// paper-thin, single-sided quad with nothing on its far side to fight; the
/// table's backing simply disappears into the wall.
pub(crate) const STATION_W_PROUD: f32 = 10.0;

/// Uniform scale of the placed wheel. `TABLE_OUTER_R` is 348 local units →
/// 348 × 0.42 ≈ 146 world — bumped from the dais-era 0.34: the wall has room
/// a floor dais didn't, and the wall-mount read wants the instrument
/// generous, not a miniature bolted to a big blank panel. Tuned to read
/// framed, not cramped, against the wall's width at the time (≈663, apothem
/// 800).
///
/// **Re-tuned for the fullscreen-panel pivot** (2026-07-10 eve, live over
/// BRP): at the old 0.42 the fullscreened W panel read as a big empty
/// screen with a small wheel floating on it — the content must OWN its
/// panel. At 0.66 the wheel spans ≈459 of the panel's 560 height (~82%),
/// and the outermost text ring (group nameplates, `GROUP_PLATE_R` ≈ 396
/// local → ≈523 placed) still clears the panel edge. Push toward ~0.76
/// only if the nameplate ring is allowed to kiss the mullions.
pub(crate) const STATION_W_SCALE: f32 = 0.66;

// ── Station E contract (the tracker pattern-grid face mounted ON the east
// wall) ──────────────────────────────────────────────────────────────────
// Tracker Station slice 0 (`snazzy-jumping-hejlsberg.md`): the same "the
// surface gets taken over by its content" call the W wheel made
// (2026-07-10) applies at E — the pattern grid mounts directly on the E
// panel rather than standing as furniture in front of it. Unlike the
// patch-bay wheel (a horizontal table re-oriented face-out), the tracker
// face is authored VERTICALLY from the start (local XY is the face plane,
// local +Z the outward normal) — `tracker::STATION_E_PLACEMENT`'s own doc
// has the resulting pitch/yaw derivation, which is simpler than W's
// (no roll tie-break needed: local +Y is already "up").

/// Face-center height on the E panel — the panel's own vertical center,
/// same convention and same number as [`STATION_W_MOUNT_Y`] (both walls
/// share `room::WALL_HEIGHT`, so both centers coincide).
pub(crate) const STATION_E_MOUNT_Y: f32 = 280.0;

/// How far the face's local origin floats proud of the E panel, along world
/// −X (into the room) — the E mirror of [`STATION_W_PROUD`]; same value,
/// same "inset-flush, part of the wall" reasoning (no coplanar-seating lift
/// needed either: the tracker face has no backing slab the way the wheel's
/// table did, it's a flat panel-mounted plate).
pub(crate) const STATION_E_PROUD: f32 = 10.0;

/// Uniform scale of the placed tracker face. Unlike the patch-bay wheel
/// (small local geometry scaled UP to fill its wall), the tracker face is
/// authored directly in world-scale units (`tracker::FACE_W`/`FACE_H` ≈
/// 994×560, matching the panel's own full width/height —
/// `bearing::octagon_panel_width` at [`WALL_APOTHEM`] × `room::WALL_HEIGHT`)
/// so this starts at `1.0` and only needs retuning live if the authored
/// face doesn't already read flush against its panel. **Amy-tunable —
/// retune live over BRP once a face is on the wall to check.**
pub(crate) const STATION_E_SCALE: f32 = 1.0;
