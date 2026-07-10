//! Shared scene palette + cross-scene geometry contracts for the kernel
//! interior (`docs/scenes/`): one hue language across the Tardis room and the
//! stations standing inside it (Amy, 2026-07-10 — "unifying styles across the
//! tardis / patch bay").
//!
//! **Material discipline** (the room's rule, now scene-family-wide): every
//! material is built-in `StandardMaterial` with `unlit: true`, brightness
//! carried in `base_color` — LDR (< 1.0 linear) reads as calm etched
//! structure, HDR (> 1.0) blooms through the app camera's threshold-1.0
//! bloom and is reserved for **live activity**. No point lights, no lit
//! metals: the 2026-07-10 tuning pass proved a ~1%-albedo metallic surface
//! swallows any lamp — emissive-on-dark is the concepts' look anyway.
//!
//! **Amendment (Amy, 2026-07-10 — "make the circuit patterns and border glow
//! faintly like the concepts... something faintly moving might be
//! interesting"):** decoration may now ALSO carry a faint, slowly moving
//! glow on top of that discipline — a traveling-wave crest (circuit traces,
//! wall trim) or a slow uniform breath (terminal pads, the inscribed ring),
//! rendered by [`crate::shaders::TraceGlowMaterial`]
//! instead of `StandardMaterial`. The crest may exceed 1.0 up to
//! [`GLOW_CREST`] (the bloom pass haloes it softly), but the element's
//! resting `trough` level times the crest stays under 1.0 — LDR on
//! time-average. Strong SUSTAINED HDR (a marker at full activity lift, the
//! console under chatter) stays reserved for live activity; the difference
//! from the faint glow is duration and cause, not the bloom threshold
//! itself.
//!
//! Hues are linear-rgb `[f32; 3]` identity colours; multiply by an LDR tier
//! before handing them to a material (`room::lin_scaled` et al.).

/// The gold — the well's reserved hue, and the room's one metal trim colour
/// (console rings, table rim, inlay rings, pylon caps, patch-bay etch). Most
/// gold trim sites tune their own flat brightness constant close to their
/// spawn site (`TABLE_GOLD_LDR`, `PYLON_CAP_GOLD_LDR`, `RING_GOLD_HUE`'s glow
/// trough…); the inscribed floor ring instead breathes via
/// [`crate::shaders::TraceGlowMaterial`] at [`GLOW_TROUGH_SUBTLE`]
/// (2026-07-10, the faint-moving-glow slice — it had been the sole user of a
/// now-removed flat `GOLD_LDR_TRIM` tier, alongside the W dais bezel, which
/// went with the dais itself in the wall-mount retune).
pub(crate) const GOLD_HUE: [f32; 3] = [1.00, 0.78, 0.34];
/// Gold etch tier: engraved guide rings, ticks — dimmer than trim so etched
/// detail supports rather than competes.
pub(crate) const GOLD_LDR_ETCH: f32 = 0.28;

/// Brass — sockets, pegs, jack hardware (warmer + dimmer than [`GOLD_HUE`];
/// the patch bay's hardware tier, formerly a lit metallic material).
pub(crate) const BRASS_HUE: [f32; 3] = [0.72, 0.55, 0.25];
pub(crate) const BRASS_LDR: f32 = 0.55;

/// Instrument working surface (the patch wheel's top): near-black on purpose
/// — the dived face reads through its gold etch and brass hardware (mockup
/// 14's gold-on-black), not through surface brightness. Only a hair above
/// the room's own furniture-surface value (`room::TABLE_COLOR`) so the
/// silhouette still separates from the wall panel behind it, now that the
/// wheel mounts flush to the W wall instead of resting on its own dais.
/// (First cut was 0.055/0.060/0.078 — live check showed a washed grey table
/// that killed the etch contrast.) `DARK_SURFACE`, this constant's former
/// palette twin, was deleted with the W dais it served
/// (`STATION_W_DAIS_TOP_Y`/`_R`/`_X`) — the 2026-07-10 wall-mount slice.
pub(crate) const DARK_SURFACE_LIFT: [f32; 3] = [0.012, 0.013, 0.019];

/// The violet family — reserved for **information** (radiators; now the
/// octagon's diagonal wall panels). Glass backdrop + thread/content strips.
pub(crate) const VIOLET_GLASS: [f32; 3] = [0.090, 0.040, 0.150];
pub(crate) const VIOLET_THREAD: [f32; 3] = [0.550, 0.180, 0.750];

// ── Faint moving glow (`TraceGlowMaterial`; Amy, 2026-07-10) ────────────────
// Shared across every glowing decoration element so one cap and one "how
// subtle is subtle" tier live in a single place, not re-guessed per site.

/// Crest ceiling: the brightest a traveling-wave crest or a breath's peak may
/// read, for ANY glowing decoration element — every element renormalizes its
/// identity hue so its brightest channel lands exactly here (`room::mod`'s
/// `crest_color`). Above 1.0 on purpose (the app's threshold-1.0 bloom pass
/// haloes it softly) but capped so decoration never reads as loud as live
/// activity.
pub(crate) const GLOW_CREST: f32 = 1.25;
/// Trough tier shared by the room's most subtle breathing elements — the
/// inscribed gold floor ring, chiefly: resting brightness stays a gentle
/// breath, never a strobe. Floor traces, terminal pads, and wall trim each
/// tune their own (livelier) trough in `room/mod.rs`, close to their site.
pub(crate) const GLOW_TROUGH_SUBTLE: f32 = 0.75;

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
/// stands on. Clears the old radiator radius (660) and the wall-station
/// radius the pylons/markers stand at (`room::ROOM_RADIUS`, 620), so the
/// shell encloses everything already standing in the room.
pub(crate) const WALL_APOTHEM: f32 = 800.0;

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
/// 348 × 0.42 ≈ 146 world, on a wall panel ≈ 663 wide
/// (`bearing::octagon_panel_width(WALL_APOTHEM)`) — bumped from the
/// dais-era 0.34: the wall has room a floor dais didn't, and the wall-mount
/// read wants the instrument generous, not a miniature bolted to a big blank
/// panel. Tuned to read framed, not cramped, against that width.
pub(crate) const STATION_W_SCALE: f32 = 0.42;
