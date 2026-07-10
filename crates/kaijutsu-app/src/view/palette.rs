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
//! Hues are linear-rgb `[f32; 3]` identity colours; multiply by an LDR tier
//! before handing them to a material (`room::lin_scaled` et al.).

/// The gold — the well's reserved hue, and the room's one metal trim colour
/// (console rings, table rim, inlay rings, pylon caps, patch-bay etch).
pub(crate) const GOLD_HUE: [f32; 3] = [1.00, 0.78, 0.34];
/// Gold trim tier: rims, caps, inlay bands.
pub(crate) const GOLD_LDR_TRIM: f32 = 0.50;
/// Gold etch tier: engraved guide rings, ticks — dimmer than trim so etched
/// detail supports rather than competes.
pub(crate) const GOLD_LDR_ETCH: f32 = 0.28;

/// Brass — sockets, pegs, jack hardware (warmer + dimmer than [`GOLD_HUE`];
/// the patch bay's hardware tier, formerly a lit metallic material).
pub(crate) const BRASS_HUE: [f32; 3] = [0.72, 0.55, 0.25];
pub(crate) const BRASS_LDR: f32 = 0.55;

/// Dark furniture surface (tabletops, pedestals, plinths, daises) — a shade
/// lighter than the room floor so mass reads against it.
pub(crate) const DARK_SURFACE: [f32; 3] = [0.032, 0.036, 0.050];
/// Instrument working surface (the patch wheel's top): one more shade up, so
/// a dived instrument face reads against its own furniture.
pub(crate) const DARK_SURFACE_LIFT: [f32; 3] = [0.055, 0.060, 0.078];

/// The violet family — reserved for **information** (radiators; now the
/// octagon's diagonal wall panels). Glass backdrop + thread/content strips.
pub(crate) const VIOLET_GLASS: [f32; 3] = [0.090, 0.040, 0.150];
pub(crate) const VIOLET_THREAD: [f32; 3] = [0.550, 0.180, 0.750];

// ── Station W contract (the patch wheel AS the west station) ────────────────
// Amy, 2026-07-10: the sign and the pylon are gone — the wheel itself is the
// station. The room builds a dais at the W bearing; the patch bay's placement
// seats the wheel on it. These constants are the agreement between the two
// files (room/mod.rs spawns the dais, patch_bay's STATION_W_PLACEMENT reads
// the same numbers), so neither can drift without the other noticing here.

/// Room-space X of the wheel's center (west is −X). The dais stands here.
pub(crate) const STATION_W_X: f32 = -400.0;
/// Uniform scale of the placed wheel. `TABLE_OUTER_R` is 348 local units →
/// ~118 world: a peer to the well table's 120, not a miniature.
pub(crate) const STATION_W_SCALE: f32 = 0.34;
/// World-Y of the dais top = the wheel table's top face (the placement's
/// translation.y — the wheel's local origin is its tabletop plane). Matches
/// the well table's ~70 top so the two instruments share a working height.
pub(crate) const STATION_W_DAIS_TOP_Y: f32 = 64.0;
/// Dais foot radius — a touch past the placed wheel's outer edge; the W
/// crimson trace bundle terminates its pads around this foot (the wiring
/// flows INTO the station).
pub(crate) const STATION_W_DAIS_R: f32 = 132.0;
