//! Shared cross-scene **geometry** contracts for the kernel interior
//! (`docs/scenes/`) — the room's octagon shell, the one datum `room/mod.rs`
//! and its wall-mounted stations have to agree on without eyeballing each
//! other.
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

// ── Octagon wall shell ───────────────────────────────────────────────────
// The one geometric datum every wall-mounted station shares with the room:
// how far out the octagon's walls stand.

/// Octagon wall apothem (center-to-face distance). `room::spawn_walls`
/// builds the panel geometry at this radius; a station mounted flush on a
/// wall panel reads the same number to seat itself against that panel.
///
/// At 1200 a panel's FULL width (`bearing::octagon_panel_width`) is
/// `2·1200·tan(π/8) ≈ 994`, against `room::WALL_HEIGHT` (560) — a 994:560 ≈
/// 16:9 frame, so `room::shot::fullscreen_pose` fills the camera's vertical
/// frustum with exactly one panel, edge to edge. Clears the old radiator
/// radius (660) and the wall-station radius the pylons/markers stand at
/// (`room::ROOM_RADIUS`, 620); the binding constraint is the octagon's own
/// circumradius (`bearing::octagon_circumradius`, ≈1299 at this apothem),
/// which must stay under `room::FLOOR_RADIUS` (1300) so the walls stand ON
/// the floor disc, not past its edge.
pub(crate) const WALL_APOTHEM: f32 = 1200.0;
