//! Room level — the shell's **Tardis chamber** (`docs/scenes/shell.md`, slice A:
//! "the room exists"). A circular vaulted room that holds the stations at
//! stable compass **bearings** around a central **console**. Left/Right cycle
//! the station carousel, Enter/Down dives (a camera zoom now, not a scene cut
//! — see Slice C below), Esc drops to the conversation.
//!
//! What slice A builds:
//! - **Geometry**: a dark floor disc inscribed with etched trace channels that
//!   bow *around* the center (never under it — the open-center rule), a subtle
//!   dark vault dome overhead, and the console emblem at center.
//! - **Bearings**: stations at their compass placement (`bearing::focus_dir`) —
//!   PatchBay W, Tracks E, VFS N, reserved S; the console is the center. The
//!   camera **dollies to face** the focused bearing (travel by intent — the
//!   same eased tween idiom as the well's `ease_camera_to_focused_ring`).
//! - **Nameplates**: engraved MSDF plates at the labeled bearings (the well's
//!   plate pipeline). Unbuilt stations stay dimmed.
//! - **Information radiators**: violet dark-glass content — now the diagonal
//!   faces of the octagon wall shell (below), not free-floating slabs.
//! - **Ambient telemetry = light**: the tracks (E) marker *breathes* with the
//!   beat (the well's [`WellBeats`] phasors, read — not re-wired), and the
//!   console emblem glows with context chatter ([`activity::BearingActivity`]).
//!   Strong sustained HDR is reserved for live activity; decoration may also
//!   carry its own FAINT, slowly moving glow (the circuit board, the wall
//!   trim — below), capped at [`ScenePalette::crest`] and LDR on
//!   time-average (Amy, 2026-07-10 — "make the circuit patterns and border
//!   glow faintly like the concepts... a lil bloom or some other shader;
//!   something faintly moving might be interesting").
//!
//! The console is the **slice-A stand-in** for the live well: an emblematic
//! gold ring-stack, *not* the well scene (unifying the well is a later slice).
//!
//! **Slice B (2026-07-09): one shared scene graph** (shell.md open question 3,
//! DECIDED). The patch bay is not a separate Bevy world reached by a scene cut —
//! it is **room furniture at the W bearing**, spawned when the room spawns
//! (`patch_bay::spawn_furniture`, under a placement entity) and alive as long as
//! `RoomRoot`. Diving is a *continuous camera descent* onto it: `enter_room` /
//! `exit_room` no longer despawn on the Room↔PatchBay hop (only leaving the shell
//! for Conversation/the well tears down), one camera + one clear colour carry
//! both screens, and the dived view earns its focus by dimming the room and
//! showing the patch bay's own LOD, not by being a different world.
//!
//! **Slice B, retuned (2026-07-10): the octagon shell + the wheel-as-station**
//! (`docs/scenes/palette.rs`'s station-W contract). The room is now enclosed by
//! eight single-sided wall panels ([`bearing::octagon_panels`]) standing on the
//! floor — the camera orbits OUTSIDE them for the overview pose, and the near
//! panel(s) cull away (default back-face culling on an inward-facing quad),
//! the dollhouse-cutaway read. The four diagonal faces carry the migrated
//! violet information threads (the old free-floating radiators). The W
//! bearing spawns no marker/plinth/cap/nameplate at all — the wheel occupies
//! it directly.
//!
//! **Wall-mount retune (same day): the wheel hangs ON the W panel.** A first
//! pass stood the wheel on a floor dais at the W bearing; Amy's call, later
//! the same day, was to mount it flush on the wall panel itself instead
//! ("the surface gets taken over by its content" — studio patch bays are
//! wall panels, not tables). The dais and its furniture builder are gone;
//! the patch bay's own placement (untouched here beyond reading
//! `palette::WALL_APOTHEM`) re-orients the wheel face-out with a pitch+yaw
//! composition and seats it flush against the panel `spawn_walls` already
//! builds — no new room-side furniture at all.
//!
//! **Slice C (2026-07-11): the time well becomes the console.** The
//! slice-A gold-ring-stack placeholder (`CONSOLE_RINGS`, the module's own old
//! "emblem of the time well" stand-in) is gone; the real well
//! (`time_well::scene::spawn_well_furniture`) now IS the console — room
//! furniture at the Center bearing, seated above the existing
//! `spawn_table` via `time_well::scene::STATION_CENTER_PLACEMENT`, the same
//! "one placement transform seats the content" contract the wheel's
//! `STATION_W_PLACEMENT` established at W. Unzoomed, the well's rings/cards
//! sit ambient and dim (matching the wheel's chords-at-rest read); Enter/Down
//! on the TimeWell carousel entry zooms the shared camera into the well's
//! mouth via [`shot::RoomShot::WellOverview`] — no scene cut — and the
//! well's own keyboard/HUD/nav take over while zoomed, exactly like the
//! wheel's dive. Ctrl+W is now a **symmetric room toggle**
//! (`time_well::scene::toggle_time_well`): Conversation → dive straight into
//! the well; Room (any station, zoomed or not) → straight back to
//! Conversation — reading only the current screen, never how it was
//! reached. `Screen::TimeWell` itself — left unreachable-but-wired by this
//! slice — is deleted cleanly by Slice D, along with its now-dead
//! `OnEnter`/`OnExit` handlers and every other call site that matched on it.
//!
//! Materials are mostly built-in [`StandardMaterial`] with `unlit: true`,
//! carrying brightness in `base_color` — LDR (< 1.0 linear) reads crisp, HDR
//! (> 1.0) blooms through the app camera's threshold-1.0 bloom
//! (`main::setup_camera`). The circuit-board traces, terminal pads, the
//! inscribed gold ring, and the wall trim instead carry
//! [`crate::shaders::TraceGlowMaterial`] (`assets/shaders/trace_glow.wgsl`)
//! — a small GPU-animated shader, the one exception to the charter's
//! procedural-first budget rule, needed because a faint MOVING glow can't be
//! baked into a static `base_color` or vertex colour. No image assets, no
//! new fonts.

pub mod activity;
pub mod bearing;
pub mod nav;
mod shot;

use std::time::Instant;

use bevy::prelude::*;

use activity::BearingActivity;
use bearing::Bearing;
use nav::{Station, StationCarousel};

use crate::connection::actor_plugin::ServerEventMessage;
use crate::shaders::{TraceGlowMaterial, WellCardMaterial};
use crate::text::ShapingFonts;
use crate::text::components::bevy_color_to_brush;
use crate::text::msdf::{
    FontDataMap, MsdfAtlas, MsdfBlockGlyphs, PositionedGlyph, collect_msdf_glyphs,
};
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};
use crate::ui::screen::Screen;
use crate::view::palette;
use crate::view::patch_bay;
use crate::view::scene_palette::{ScenePalette, lin_scaled};
use crate::view::time_well::live::WellBeats;
use crate::view::time_well::panel::{commit_panel_glyphs, create_msdf_panel};
use vello::peniko::Brush;

// ── Room geometry (Amy-tunable) ─────────────────────────────────────────────
// Color/brightness constants moved onto `Res<ScenePalette>`
// (feat/scene-palette-migration) — see `palette.rs`'s header. What's left
// here is geometry only.

/// Floor disc radius (world units); comfortably past the wall stations so the
/// room reads as a chamber, not a platform. Bumped 1100 → 1300 alongside
/// `palette::WALL_APOTHEM`'s 800 → 1200 (2026-07-10 evening, the
/// fullscreen-panel pivot): must clear the octagon's own circumradius
/// (`bearing::octagon_circumradius(palette::WALL_APOTHEM)` ≈ 1299) so the
/// walls stand ON the floor disc rather than past its edge —
/// `floor_radius_clears_the_octagon_circumradius_so_the_walls_stand_on_the_floor`
/// locks the margin.
const FLOOR_RADIUS: f32 = 1300.0;
/// Floor mesh resolution ([`bearing::disc_vertices`]): concentric rings ×
/// angular segments. Coarse enough to stay cheap, fine enough that the radial
/// gradient ([`bearing::floor_color`]) reads smooth, not banded.
const FLOOR_RINGS: usize = 14;
const FLOOR_SEGMENTS: usize = 64;

/// Enclosing vault-dome radius. Must exceed **every** camera distance (the
/// pulled-back overview sits ~2093 out, since `shot::OVERVIEW_POS` was pulled
/// back alongside the 2026-07-10 evening apothem bump) so the camera stays
/// *inside* the dome and its far inner surface reads as the vault; the lower
/// hemisphere hides under the floor disc.
const DOME_RADIUS: f32 = 2600.0;

/// Radius of the wall stations' marker pylons.
const ROOM_RADIUS: f32 = 620.0;
/// Radius of the nameplates — a touch inside the pylons so a plate floats in
/// front of its station.
const PLATE_RADIUS: f32 = 560.0;

/// Central keep-out radius — every floor trace stays outside it, so no trace
/// crosses the console (the open-center rule, `shell.md`). Enforced by a
/// `debug_assert` as each trace spawns.
const KEEPOUT_RADIUS: f32 = 150.0;

/// Marker pylon dimensions (a slim square post standing on the floor).
const MARKER_WIDTH: f32 = 26.0;
const MARKER_HEIGHT: f32 = 260.0;
/// Reserved-bearing (South) marker height — roughly a third of a built
/// station's pylon, so the empty plot reads as a low waymarker post rather
/// than a monolith standing in the overview shot's near foreground (the
/// south-marker-blocks-the-overview bug, `shell.md` open question 1). Still
/// tall enough to read as "reserved, not vanished."
const MARKER_HEIGHT_RESERVED: f32 = MARKER_HEIGHT / 3.0;

// ── Pylon furniture (Amy-tunable) ────────────────────────────────────────────

/// Every bearing pylon gets a wider low plinth grounding it to the floor.
const PYLON_PLINTH_WIDTH: f32 = MARKER_WIDTH * 2.4;
const PYLON_PLINTH_HEIGHT: f32 = 18.0;
/// A gold cap slab crowns every BUILT station's pylon ([`wants_gold_cap`]).
/// The reserved South marker gets a plinth only — it stays humble, a stub for
/// a station that doesn't exist yet, not dressed like one that does.
const PYLON_CAP_WIDTH: f32 = MARKER_WIDTH * 1.6;
const PYLON_CAP_HEIGHT: f32 = 14.0;
// The plinth reads `ScenePalette::table` directly (it was a bare alias for
// `TABLE_COLOR`); the cap reads `ScenePalette::gold` × `ScenePalette::trim` —
// "the same gold family/weight as the table rim and console rings, so every
// gold accent in the room reads as one hue."

/// Nameplate quad (world units) and its texture (logical px) — the well's plate
/// grammar, kept API-compatible for the patch bay's own plates.
const PLATE_QUAD_W: f32 = 210.0;
const PLATE_QUAD_H: f32 = 62.0;
pub(crate) const PLATE_TEX_W: f32 = 340.0;
pub(crate) const PLATE_TEX_H: f32 = 100.0;
pub(crate) const PLATE_PAD: f32 = 10.0;
pub(crate) const PLATE_FONT_SIZE: f32 = 30.0;
/// Height (world-Y) the nameplates float at — hung high near the pylon tops
/// (concept 06's signage grammar: plates over the stations, furniture below;
/// also clears the W approach's sight line to the patch-bay table).
const PLATE_HEIGHT: f32 = 200.0;
// `APPROACH_LOOK_HEIGHT` (where the approach pose *looks*, world-Y at the
// wall) moved to `shot.rs` — pure camera framing, no longer read here.

// ── Well table (Amy: "MORE SOLIDNESS" — heavy furniture, not hologram) ──────

/// The table the console rings hover above: a chunky tabletop, a gold rim,
/// a narrower pedestal, and a wide low plinth grounding it all to the floor.
/// `pub(crate)` since Slice C (time-well/room integration plan): the well's
/// own `STATION_CENTER_PLACEMENT` (`time_well/scene.rs`) reads this to seat
/// its ring stack above the same table face, the same cross-module contract
/// `palette::STATION_W_MOUNT_Y`/`WALL_APOTHEM` already establish for the
/// patch bay's wall placement.
pub(crate) const TABLE_TOP_Y: f32 = 70.0;
const TABLE_RADIUS: f32 = 120.0;
const TABLE_THICKNESS: f32 = 14.0;
/// Gold rim torus at the tabletop's edge.
const TABLE_RIM_MINOR: f32 = 7.0;
/// Pedestal — narrower than the tabletop, rising from the plinth.
const TABLE_PEDESTAL_RADIUS: f32 = 55.0;
/// Plinth — wider than the tabletop, grounding it to the floor. Sized to
/// just clear [`KEEPOUT_RADIUS`] from the inside: the table's foot fills
/// almost exactly the circle the floor traces are forbidden from crossing —
/// the traces stay clear of the console because the table is *physically
/// standing there*, not by an arbitrary rule.
const TABLE_PLINTH_RADIUS: f32 = 145.0;
const TABLE_PLINTH_HEIGHT: f32 = 16.0;
// Table/pedestal/plinth colour (`ScenePalette::table`) and the rim torus's
// gold trim brightness (`ScenePalette::gold` × `ScenePalette::trim`) moved
// onto the palette resource.

// ── Octagon wall shell (the enclosing chamber; `shell.md`'s cutaway
// centerpiece) ──────────────────────────────────────────────────────────────
// Eight flat single-sided panels forming a wall around the room — faces on
// the four cardinals plus the four `bearing::RADIATOR_DIRS` diagonals,
// `palette::WALL_APOTHEM` out from center ([`bearing::octagon_panels`]).
// Every panel/mullion/trim/thread quad MUST stay single-sided (default
// `cull_mode`, no `Cuboid`, no `cull_mode: None`): a camera outside the
// octagon sees a near panel's back face — culled — and the chamber shows
// through (the dollhouse cutaway; see `bearing`'s own module comment for the
// exact mechanics). No W-bearing dais stands here any more (the 2026-07-10
// wall-mount retune, `palette.rs`'s "Station W contract"): the wheel mounts
// directly on the W panel below via `patch_bay::STATION_W_PLACEMENT`, which
// is why `WALL_APOTHEM` itself now lives in `palette.rs` — both files read
// the same number, this one just for the panel geometry.

/// Panel height, standing on the floor (`y = 0` to `WALL_HEIGHT`).
const WALL_HEIGHT: f32 = 560.0;
/// Reveal subtracted from [`bearing::octagon_panel_width`]'s full (corner-
/// touching) width so a corner mullion has room to stand without z-fighting
/// its neighbours' trimmed edges.
const WALL_PANEL_GAP: f32 = 40.0;
/// Corner mullion strip width — the old marker pylons' own post scale
/// ([`MARKER_WIDTH`]), so the shell's architecture and the furniture standing
/// in front of it read as one family.
const WALL_MULLION_WIDTH: f32 = MARKER_WIDTH;
// Panel base colour (`ScenePalette::wall_base` — a hair lighter than the
// dome's rim, [`bearing::dome_color`]`(0.0)`, so a panel reads as a surface
// catching the vault's glow, not a hole in it) and mullion colour
// (`ScenePalette::wall_mullion` — a shade darker than the panel base) moved
// onto the palette resource.
/// Edge-trim strip thickness, and how far it floats proud of the panel base
/// (inward, along the panel's own local +Z — the "proud" idiom the old
/// radiator thread-strips used, now needed for the trim too since both are
/// now zero-thickness quads that would otherwise share a plane).
const WALL_TRIM_THICKNESS: f32 = 12.0;
const WALL_TRIM_PROUD: f32 = 2.2;
// Trim glow trough (`ScenePalette::trough_wall_trim` — restrained neon at
// rest, not a blown highlight; mission's "LDR ~0.5-0.7 of hue"; `trough *
// ScenePalette::crest` stays under 1.0) moved onto the palette resource. The
// trim carries a [`TraceGlowMaterial`] traveling wave (mode 0, `spawn_walls`)
// whose crest renormalizes to `ScenePalette::crest` — the trough is its
// RESTING level between passes.
/// One crest crosses a whole trim strip roughly every this many seconds
/// (mode 0 period, `rate = 1 / WALL_TRIM_GLOW_PERIOD`) — long and slow, per
/// panel (`spawn_walls`'s per-panel phase keeps the eight panels from
/// pulsing in lockstep).
const WALL_TRIM_GLOW_PERIOD: f32 = 10.0;

/// Diagonal-panel content: the violet information threads migrated in from
/// the old free-floating radiators (`shell.md`, "the walls between bearings
/// are information radiators") — now rendered directly on the diagonal wall
/// panel's inward face. Jitter ranges and hue carry over from the old
/// `RADIATOR_THREAD_*` constants (deleted), just retargeted from a slab's
/// local size to the wall's, and a few more strips for the bigger surface.
const WALL_THREAD_COUNT: usize = 12;
const WALL_THREAD_WIDTH: f32 = 5.0;
/// Proud offset for the content threads — a touch more than the trim's, so
/// threads read as the panel's foreground detail over the trimmed frame.
const WALL_THREAD_PROUD: f32 = 2.6;
/// `(min, max)` thread height as a fraction of [`WALL_HEIGHT`].
const WALL_THREAD_HEIGHT_RANGE: (f32, f32) = (0.30, 0.80);
const WALL_THREAD_BRIGHTNESS_RANGE: (f32, f32) = (0.3, 0.9);

// ── Circuit-board floor (the wiring — static LDR engravings; Amy-tunable) ───

/// Trace ribbon height above the floor (avoids z-fighting) and base width
/// (each route scales this by its own `width_scale`, 0.5–1.2).
const TRACE_Y: f32 = 0.6;
const TRACE_WIDTH: f32 = 7.0;
// Etched fabric hues (crimson = MIDI, cyan = PCM, green = VFS, gold = the
// well) moved onto `ScenePalette::trace_crimson`/`trace_cyan`/`trace_green`/
// `trace_gold`; the violet stubs read `ScenePalette::violet_glass` directly
// (`route_bundles`, below). At rest a trace is a dark engraving; it lights
// HDR only when its flow runs (later slices). One hue family per fabric (the
// charter's rainbow-board rule).

/// Inscribed gold ring routes depart from (concept 06's gold circle band):
/// center radius and band width. Clears [`KEEPOUT_RADIUS`] with room to
/// spare. Was a double-ring; the inner one came out 2026-07-12 (Amy) — with
/// the well console wearing its own gold at center, three nested gold
/// circles read as clutter.
const RING_OUTER_R: f32 = 230.0;
const RING_BAND_WIDTH: f32 = 10.0;
// The inscribed ring's gold hue used to be its own near-duplicate constant
// (`RING_GOLD_HUE = [1.00, 0.80, 0.36]`, ~2% off `CONSOLE_GOLD_HUE`'s
// [1.00, 0.78, 0.34]) — collapsed onto `ScenePalette::gold` (the ONE
// sanctioned value change in this migration; every gold accent in the room
// now reads the identical hue, not two eyeballed near-twins).
/// Slow breathing rate (mode 1, rad/s) for the room's calmest gold glow
/// accent — the inscribed floor ring: subtle, architectural, unhurried.
/// (Once shared with the W dais bezel, retired in the 2026-07-10
/// wall-mount slice along with the dais itself.)
const GOLD_GLOW_RATE: f32 = 0.15;

/// Route-generation shared geometry ([`bearing::expand_bundle`]): the radial
/// length each 45°-ish bend cuts off the lane radius, and the arc's sample
/// density.
const ROUTE_CHAMFER: f32 = 18.0;
const ROUTE_ARC_SEGMENTS: usize = 14;
/// Terminal-pad disc radius range (world units).
const ROUTE_PAD_RADIUS_RANGE: (f32, f32) = (6.0, 14.0);

// ── Faint moving glow (`TraceGlowMaterial`; Amy, 2026-07-10) ────────────────
// "make the circuit patterns and border glow faintly like the concepts... a
// lil bloom or some other shader; something faintly moving might be
// interesting." Every crest renormalizes to `ScenePalette::crest`
// (`crest_color`); these constants are each element's RESTING trough and
// its wave/breath rate — the knobs that make one glowing thing read subtler
// or livelier than another. SLOW and FAINT is the target across the board —
// the room must not read as an arcade.

// Floor-trace glow trough (mode 0, `spawn_floor`: `ScenePalette::trough_wiring`
// — the resting brightness fraction of a route's crest colour between the
// traveling crest's passes, a clear step down so the crest's transit reads
// as motion, not a flicker) and terminal-pad glow trough (mode 1,
// `ScenePalette::trough_pads` — a hair brighter than the wiring trough, since
// every crest shares the same `ScenePalette::crest` ceiling regardless of
// gain) moved onto the palette resource.
/// `(min, max)` seconds for one crest to cross a whole route (mode 0 period,
/// `rate = 1 / period`) — jittered per-route from `bearing::BoardRoute`'s
/// `glow_phase01` so the board doesn't pulse in lockstep.
const TRACE_GLOW_PERIOD_RANGE: (f32, f32) = (8.0, 14.0);
/// Terminal-pad breathing rate (mode 1, rad/s) — slow and uniform; pads
/// don't jitter their rate the way traces jitter their period; only phase
/// (from the same hash stream as their trace) varies pad-to-pad.
const PAD_GLOW_RATE: f32 = 0.25;

// ── Focus presentation ───────────────────────────────────────────────────────

/// Plate brightness ([`WellCardMaterial::dim`].x) by focus/built state.
const PLATE_DIM_FOCUSED: f32 = 1.0;
const PLATE_DIM_IDLE: f32 = 0.45;
const PLATE_DIM_UNBUILT: f32 = 0.22;
const PLATE_SCALE_FOCUSED: f32 = 1.18;
/// Brass border for the focused plate (the engraved-nameplate read).
const PLATE_BORDER_FOCUSED: Vec4 = Vec4::new(0.85, 0.65, 0.25, 1.0);
/// Plate body fill.
const PLATE_ACCENT: Vec4 = Vec4::new(0.075, 0.085, 0.125, 1.0);

// ── Camera (travel by intent — the well's tween idiom) ───────────────────────
//
// The pose math itself (approach-pose eye radius/height, the console
// overview position, the fullscreen-panel geometry) moved to `shot.rs`
// (2026-07-11, Slice A of the time-well/room integration — a pure
// `RoomShot`/`resolve` table, unit-tested there instead of here). This
// section keeps only what's still genuinely `ease_shell_camera`'s own: the
// tween rate.

/// Camera follow rate (exponential smoothing) — matches the well's weighty
/// glide so the shell and the well feel like one instrument.
const CAMERA_EASE_RATE: f32 = 4.0;

// ── Ambient glow gains ───────────────────────────────────────────────────────
// Marker rest brightness (`ScenePalette::marker`), tracks (E) beat gain
// (`ScenePalette::gain_beat` — `global_envelope` (0..1) → this much HDR lift
// on the tracks marker each beat, the acceptance "breathe," `shell.md` slice
// A), the sustained lift under the beat while a track is rolling
// (`ScenePalette::gain_active`), and the steady brightness lift on the
// focused station's marker/console (`ScenePalette::gain_focus_lift`) all
// moved onto the palette resource.

/// Quantization step for the glow lanes — coarse enough that a settled marker
/// stops re-extracting its material (the well's `LIVE_LANE_STEP` discipline).
const GLOW_STEP: f32 = 1.0 / 64.0;

// ── State ─────────────────────────────────────────────────────────────────────

/// Which station the room carousel focuses. Whoever *enters* the room sets the
/// focus first (the well focuses TIME WELL; the patch bay focuses PATCH BAY),
/// so arriving always faces where you came from.
#[derive(Resource)]
pub struct RoomState {
    pub carousel: StationCarousel,
    /// The station the camera is fullscreened onto, or `None` at room scale
    /// (2026-07-10 evening, the fullscreen-panel pivot — supersedes the old
    /// `Screen::PatchBay` state entirely: "diving" is now a camera pose plus
    /// this field, not a screen). Only [`station_is_zoomable`] stations ever
    /// occupy it; `room_keyboard` sets it on Enter/Down, clears it on Esc/Up,
    /// and `exit_room` clears it unconditionally on the way out of the room
    /// so a later re-entry always starts unzoomed. Reading this (not a
    /// `State<Screen>`) is what `ease_shell_camera`, `apply_room_dive_visibility`,
    /// and the patch bay's own LOD/keyboard gates key off now.
    pub zoomed: Option<Station>,
    /// Re-lay-out the nameplates on the next frame (the patch bay's
    /// `text_dirty` shape — `view/patch_bay/mod.rs`). `StationPlate` entities
    /// live for ONE room visit: `exit_room` despawns `RoomRoot` (cascading to
    /// every plate), `enter_room` respawns fresh, glyph-less ones — but this
    /// `RoomState` resource survives every visit. A process-lifetime "done"
    /// latch (the shape this flag replaced) fills the *first* visit's plates
    /// and then leaves every later visit blank forever, because the entities
    /// it thinks it already filled are long gone. The arm has to be
    /// per-entry: set by [`arm_on_enter`], cleared by [`room_plate_text`]
    /// only once it actually commits glyphs.
    plates_dirty: bool,
}

impl Default for RoomState {
    fn default() -> Self {
        Self { carousel: StationCarousel::new(Station::TimeWell), zoomed: None, plates_dirty: true }
    }
}

// `WellEdgeBump` (the Up-Up speedbump at the well's mouth ring) is retired
// entirely (Slice C, `lovely-swimming-prism.md`): it only existed because the
// well was a separate screen BELOW the room; now that the well IS the room's
// center furniture, "leave the well" is just `room.zoomed = None`, the same
// generic zoom-out every `station_is_zoomable` station uses.

// ── Components ────────────────────────────────────────────────────────────────

/// Root of all room entities (despawn is recursive).
#[derive(Component)]
pub struct RoomRoot;

/// Marks the shared app camera while the room owns it.
#[derive(Component)]
pub struct RoomCamera;

/// One station nameplate; `0` indexes [`Station::ALL`].
#[derive(Component)]
pub struct StationPlate(pub usize);

/// A wall bearing's marker pylon: its bearing, its identity hue (linear rgb),
/// and the station standing there (if any). The glow system lifts the tracks
/// (E) marker with the beat and lifts whichever is focused.
#[derive(Component)]
pub struct BearingMarker {
    pub bearing: Bearing,
    pub hue: Vec3,
    pub station: Option<Station>,
}

// `ConsoleEmblem` (the slice-A gold-ring placeholder's marker component) is
// retired alongside `CONSOLE_RINGS` (Slice C) — the real time well now stands
// at the console bearing; its own components (`time_well::scene::Card`,
// `TerraceRing`, …) carry whatever chatter-glow the well itself wires up.

/// Room chrome that **fades when you zoom** into a station (`docs/scenes/shell.md`,
/// slice B; the fullscreen-panel pivot, 2026-07-10 evening): the bearing pylons,
/// station nameplates, console emblem, and violet radiators. `apply_room_dive_visibility`
/// hides these while [`RoomState::zoomed`] is `Some` so the fullscreened station
/// owns the eye; the floor, its traces, the vault dome, and the zoomed station
/// itself stay — they are the chamber, not distractions.
#[derive(Component)]
pub struct RoomDistraction;

// ── Plugin ────────────────────────────────────────────────────────────────────

pub struct RoomPlugin;

impl Plugin for RoomPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RoomState>()
            .init_resource::<BearingActivity>()
            .add_plugins(MaterialPlugin::<TraceGlowMaterial>::default())
            .add_systems(OnEnter(Screen::Room), enter_room)
            .add_systems(OnExit(Screen::Room), exit_room)
            // Ambient ingest runs on **every** screen so the room opens warm —
            // the same rationale as `time_well::live::ingest_live_events` (both
            // resources stay current while you're elsewhere). Bounded to five
            // bearings.
            .add_systems(Update, ingest_room_activity)
            .add_systems(
                Update,
                (room_keyboard, room_plate_text, room_focus_visuals, sync_room_glow)
                    .chain()
                    .run_if(in_state(Screen::Room)),
            )
            // The camera dolly retargets the moment `RoomState::zoomed` flips so
            // diving/surfacing reads as one continuous move, and the dive dims the
            // room chrome. Both read `zoomed` rather than a separate screen now
            // (`docs/scenes/shell.md`'s "one shared scene graph" decision, taken
            // one step further 2026-07-10 evening: there's no second screen left
            // to run across).
            .add_systems(
                Update,
                (ease_shell_camera, apply_room_dive_visibility).run_if(in_state(Screen::Room)),
            );
    }
}

// ── Enter / exit ──────────────────────────────────────────────────────────────

/// Force a fresh nameplate layout for the plates this call to `enter_room` is
/// about to spawn. `RoomState` is a `Resource` — it survives `exit_room`,
/// which only despawns `RoomRoot` (and with it every `StationPlate`);
/// without re-arming `plates_dirty` here, a second (or later) visit finds it
/// already cleared from the first and `room_plate_text` never fills the
/// fresh, glyph-less plates just spawned — the blank-nameplate-on-re-entry
/// bug this arm fixes. Mirrors patch_bay's `arm_on_enter`.
fn arm_on_enter(room: &mut RoomState) {
    room.plates_dirty = true;
}

fn enter_room(
    mut commands: Commands,
    mut room: ResMut<RoomState>,
    mut pb_state: ResMut<patch_bay::PatchBayState>,
    mut well_state: ResMut<crate::view::time_well::scene::TimeWellState>,
    mut well_tracks: ResMut<crate::view::time_well::rays::WellTracks>,
    palette: Res<ScenePalette>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut mats: ResMut<Assets<StandardMaterial>>,
    mut glow_mats: ResMut<Assets<TraceGlowMaterial>>,
    mut card_mats: ResMut<Assets<WellCardMaterial>>,
    mut ring_mats: ResMut<Assets<crate::shaders::WellRingsMaterial>>,
    mut terrace_mats: ResMut<Assets<crate::shaders::TerraceRingMaterial>>,
    mut images: ResMut<Assets<Image>>,
    // Camera + Transform + Projection folded into ONE query (not a separate
    // `Query<(Entity, &Projection), _>` alongside it) to stay under Bevy's
    // 16-param ceiling for a function-as-system — `enter_room` was the first
    // system in this codebase to actually hit it (Slice C added enough new
    // room-furniture params to tip it over).
    mut app_camera: Query<(Entity, &mut Camera, &mut Transform), With<Camera3d>>,
    existing: Query<Entity, With<RoomRoot>>,
) {
    // Defensive belt-and-braces, not a live path today: `OnEnter(Screen::Room)`
    // only fires on an actual `Screen` transition into `Room` (Bevy states
    // no-op a `set()` to the state already active), and diving/surfacing no
    // longer touches `Screen` at all since `RoomState::zoomed` replaced
    // `Screen::PatchBay` — so nothing should ever call this while `RoomRoot`
    // is still standing. Kept anyway so a stale root can never get a second
    // one spawned on top of it.
    if !existing.is_empty() {
        return;
    }

    arm_on_enter(&mut room);

    // Claim the shared app camera (the well-marker convention: insert to claim,
    // remove + restore clear color to release) and place it facing the entering
    // focus so there's no first-frame snap before the ease takes over.
    if let Ok((cam_entity, mut cam, mut tf)) = app_camera.single_mut() {
        commands.entity(cam_entity).insert(RoomCamera);
        cam.clear_color = ClearColorConfig::Custom(Color::LinearRgba(palette.bg));
        let (pos, look) = shot::resolve(shot::RoomShot::focused(room.carousel.focused_station()));
        *tf = Transform::from_translation(pos).looking_at(look, Vec3::Y);
    }

    let root = commands
        .spawn((RoomRoot, Transform::default(), Visibility::Inherited, Name::new("RoomRoot")))
        .id();

    // Floor disc, inscribed gold ring, and circuit-board routes — one cohesive
    // helper (`shell.md`, "the floor is the wiring": the disc, its rings, and
    // its traces are the chamber, not room chrome).
    spawn_floor(&mut commands, root, &palette, &mut meshes, &mut mats, &mut glow_mats);

    // Vault dome — an enclosing sphere with a subtle vertical vertex-colour
    // gradient (calm darkness overhead; `shell.md` open question 4 defers the
    // dome's content — no starfield). Viewed from inside → no back-face cull.
    let dome_mat = mats.add(StandardMaterial {
        base_color: Color::WHITE, // vertex colours carry the gradient
        unlit: true,
        cull_mode: None,
        ..default()
    });
    commands.spawn((
        Mesh3d(meshes.add(dome_mesh(DOME_RADIUS))),
        MeshMaterial3d(dome_mat),
        Transform::default(),
        Visibility::Inherited,
        Name::new("RoomVault"),
        ChildOf(root),
    ));

    // The well table — heavy furniture, not hologram (Amy). Spawned before the
    // ring stack it stands under; render order doesn't matter (depth-tested),
    // this just keeps "table, then what stands on it" readable in the diff.
    spawn_table(&mut commands, root, &palette, &mut meshes, &mut mats);

    // The time well itself — room furniture at the Center bearing (Slice C,
    // `lovely-swimming-prism.md`), replacing the slice-A `CONSOLE_RINGS`
    // placeholder this loop used to spawn. Seated above the table's top face
    // via `STATION_CENTER_PLACEMENT`; `arm_well` re-arms the per-visit join
    // state the same frame (the `patch_bay::arm_scene` analog just below).
    crate::view::time_well::scene::spawn_well_furniture(
        &mut commands,
        Some(root),
        &mut well_state,
        &palette,
        &mut meshes,
        &mut card_mats,
        &mut ring_mats,
        &mut terrace_mats,
        &mut images,
    );
    crate::view::time_well::scene::arm_well(&mut well_state, &mut well_tracks);

    // Wall stations: a marker pylon at each bearing, plus an engraved nameplate
    // at the labeled ones (the reserved South bearing gets a dim marker only).
    // A furnished bearing (`bearing::station_is_room_furniture` — today just
    // PatchBay/W, "the wheel IS the west station") gets neither: no marker, no
    // plate — the wheel mounts directly on the wall panel instead (no
    // room-side furniture builder needed for it at all any more).
    for wp in bearing::wall_placements() {
        if wp.station.is_some_and(bearing::station_is_room_furniture) {
            continue;
        }

        let hue = Vec3::from_array(wp.hue);
        // The reserved South bearing (no station) gets a low stub — tall
        // enough to read as "reserved", short enough to stop standing in the
        // overview shot's near foreground (MARKER_HEIGHT_RESERVED).
        let marker_h = if wp.station.is_some() { MARKER_HEIGHT } else { MARKER_HEIGHT_RESERVED };
        let marker_mesh = meshes.add(Cuboid::new(MARKER_WIDTH, marker_h, MARKER_WIDTH));
        let marker_mat = mats.add(unlit(lin_v(hue * palette.marker)));
        // Seated ON the plinth (spawn_pylons), not interpenetrating it — the
        // shared-family dark colours hid the old overlap, but a contrasting
        // plinth tune would have exposed z-fighting (kaibo, 2026-07-10).
        let marker_pos = Vec3::new(
            wp.dir[0] * ROOM_RADIUS,
            PYLON_PLINTH_HEIGHT + marker_h * 0.5,
            wp.dir[2] * ROOM_RADIUS,
        );
        commands.spawn((
            Mesh3d(marker_mesh),
            MeshMaterial3d(marker_mat),
            Transform::from_translation(marker_pos),
            BearingMarker { bearing: wp.bearing, hue, station: wp.station },
            RoomDistraction,
            Visibility::Inherited,
            Name::new(format!("BearingMarker-{:?}", wp.bearing)),
            ChildOf(root),
        ));

        if let Some(station) = wp.station {
            let idx = Station::ALL.iter().position(|&s| s == station).unwrap_or(0);
            let plate_mesh = meshes.add(Rectangle::new(PLATE_QUAD_W, PLATE_QUAD_H));
            let (image, panel) =
                create_msdf_panel(&mut images, PLATE_TEX_W as u32, PLATE_TEX_H as u32);
            let material = card_mats.add(WellCardMaterial {
                texture: image,
                accent: PLATE_ACCENT,
                params: Vec4::ZERO,
                shape: plate_shape(),
                border: Vec4::ZERO,
                // dim.x only — .y/.z are the well's live chatter/beat lanes;
                // leaving them nonzero paints the accidental cyan+gold ring.
                dim: Vec4::new(PLATE_DIM_IDLE, 0.0, 0.0, 0.0),
            });
            let plate_pos =
                Vec3::new(wp.dir[0] * PLATE_RADIUS, PLATE_HEIGHT, wp.dir[2] * PLATE_RADIUS);
            // Face inward: aim -Z outward (2·pos − center at plate height) so the
            // visible +Z face points at the console — toward the orbiting camera.
            let outward = Vec3::new(plate_pos.x * 2.0, PLATE_HEIGHT, plate_pos.z * 2.0);
            commands.spawn((
                StationPlate(idx),
                RoomDistraction,
                Mesh3d(plate_mesh),
                MeshMaterial3d(material),
                Transform::from_translation(plate_pos).looking_at(outward, Vec3::Y),
                Visibility::Inherited,
                panel,
                Name::new(format!("StationPlate-{}", station.label())),
                ChildOf(root),
            ));
        }
    }

    // Pylon plinths + gold caps — the plain marker posts get grounded furniture
    // (`shell.md`'s "the atrium rules" read); the reserved South stub stays
    // plinth-only ([`wants_gold_cap`]). Skips the furnished W bearing same as
    // the marker/plate loop above.
    spawn_pylons(&mut commands, root, &palette, &mut meshes, &mut mats);

    // No W-bearing furniture spawns here any more (the 2026-07-10 wall-mount
    // retune): the wheel mounts directly on the W wall panel `spawn_walls`
    // builds below, via `patch_bay::STATION_W_PLACEMENT` — nothing for the
    // room side to build or ground.

    // The octagon wall shell: eight single-sided panels enclosing the room,
    // corner mullions, hue-coded edge trim, and — on the four diagonals — the
    // violet information threads that used to stand as free-floating
    // radiators. The chamber, not room chrome (no RoomDistraction).
    spawn_walls(&mut commands, root, &palette, &mut meshes, &mut mats, &mut glow_mats);

    // Re-root the patch bay into the room as furniture at the W bearing (slice B,
    // one shared scene graph). It rides `RoomRoot`, so it lives exactly as long as
    // the room; `arm_scene` primes the first observed-graph poll so its chords —
    // the W ambient — build straight away without a dive.
    patch_bay::spawn_furniture(
        &mut commands,
        root,
        &palette,
        &mut meshes,
        &mut mats,
        &mut card_mats,
        &mut images,
    );
    patch_bay::arm_scene(&mut pb_state);

    info!("room: entered (Tardis chamber — patch bay at W, the time well at center)");
}

pub(crate) fn exit_room(
    mut commands: Commands,
    mut room: ResMut<RoomState>,
    theme: Res<crate::ui::theme::Theme>,
    roots: Query<Entity, With<RoomRoot>>,
    mut app_camera: Query<(Entity, &mut Camera), With<RoomCamera>>,
    well_legend: Query<Entity, With<crate::view::time_well::legend::WellLegend>>,
) {
    // Unconditional (2026-07-10 evening, the fullscreen-panel pivot): diving
    // used to be a second screen (`Screen::PatchBay`) sharing this scene
    // graph, so leaving the shell FROM a dive could bypass this very
    // `OnExit(Screen::Room)` — the state being left was `PatchBay`, not
    // `Room` — and the dive's own exit had to duplicate this teardown or leak
    // the room. Dissolving that state removes the whole class of bug: diving
    // is now a `RoomState::zoomed` write, not a screen, so `Screen::Room` is
    // the ONLY state this scene graph ever occupies, and every way out of it
    // runs this one exit. Clear the zoom too, so a later re-entry always
    // starts unzoomed rather than inheriting a stale target from a
    // long-despawned visit.
    room.zoomed = None;
    teardown_room(&mut commands, &theme, &roots, &mut app_camera);
    // The transient legend, when up, is a `Camera3d` child, NOT a `RoomRoot`
    // child — `teardown_room`'s recursive despawn above never touches it, so
    // it must be despawned explicitly here or it survives across room visits,
    // doubling up on the next `enter_room` (well, it would toggle instead,
    // but living past its own room visit is still a leak).
    for e in well_legend.iter() {
        commands.entity(e).despawn();
    }
    info!("room: exited");
}

/// Tear the room down: despawn `RoomRoot` (recursively — the chamber and all
/// its furniture, the W patch bay included) and release the shared camera
/// (drop the [`RoomCamera`] claim, restore the conversation clear colour).
/// Kept as its own helper for readability even though [`exit_room`] is its
/// only caller now (`docs/scenes/shell.md`'s "one shared scene graph" no
/// longer has a second exit to share it with).
pub(crate) fn teardown_room(
    commands: &mut Commands,
    theme: &crate::ui::theme::Theme,
    roots: &Query<Entity, With<RoomRoot>>,
    app_camera: &mut Query<(Entity, &mut Camera), With<RoomCamera>>,
) {
    for e in roots.iter() {
        commands.entity(e).despawn();
    }
    if let Ok((cam_entity, mut cam)) = app_camera.single_mut() {
        commands.entity(cam_entity).remove::<RoomCamera>();
        cam.clear_color = ClearColorConfig::Custom(theme.bg);
    }
}

// ── Spawn helpers (called from `enter_room`) ─────────────────────────────────

/// The circuit-board route bundles — the rainbow-board authoring table
/// (`shell.md`, "the floor is the wiring"): crimson (MIDI) toward the W/E
/// patch-bay↔tracks axis, VFS green and cyan (PCM) toward N, violet short
/// stubs fanning the four radiator diagonals, and a couple of well-gold
/// routes sparingly toward the reserved S quadrant (gold is the console's
/// hue, not the floor's). ~24–36 total routes ([`bearing::expand_bundle`]
/// expands each bundle's `count`). Angles are read straight off
/// [`Bearing::dir`] via [`bearing::dir_theta`] so a re-placed bearing can't
/// silently drift out of sync with its floor traces. **Amy-tunable.**
///
/// The W bundle's `pad_range` was retuned three times on 2026-07-10 (`shell.md`,
/// "the wheel IS the west station"): first to cluster just past the W dais's
/// foot, then for the wall-mount retune (the dais is gone, and the wiring
/// flows all the way to the wall the wheel hangs on, terminating at the
/// panel's base instead of the old floor-furniture foot), then again that
/// evening when the octagon itself grew (`palette::WALL_APOTHEM` 800 → 1200,
/// the fullscreen-panel pivot) — expressed as `WALL_APOTHEM` minus a fixed
/// gap (160/30) rather than a re-guessed literal, so the pads stay the SAME
/// distance short of the wall regardless of how far out the wall itself
/// stands. The other wall-reaching bundles (E crimson, N green, N cyan, S
/// gold) stretch their `pad_range` by the same ratio the floor itself grew
/// (`FLOOR_RADIUS` 1100 → 1300, ×13/11) so the whole board still reaches
/// proportionally toward the bigger room — the violet diagonal stubs don't
/// reach a wall at all (they depart and land near the inscribed ring) and are
/// untouched.
fn route_bundles(palette: &ScenePalette) -> [bearing::RouteBundle; 9] {
    use bearing::{Bearing, RouteBundle, dir_theta};
    let west = dir_theta(Bearing::West.dir());
    let east = dir_theta(Bearing::East.dir());
    let north = dir_theta(Bearing::North.dir());
    let south = dir_theta(Bearing::South.dir());
    let [ne, se, sw, nw] = bearing::RADIATOR_DIRS.map(dir_theta);
    let trace_crimson = ScenePalette::vec3(palette.trace_crimson).to_array();
    let trace_green = ScenePalette::vec3(palette.trace_green).to_array();
    let trace_cyan = ScenePalette::vec3(palette.trace_cyan).to_array();
    let trace_gold = ScenePalette::vec3(palette.trace_gold).to_array();
    let violet_glass = ScenePalette::vec3(palette.violet_glass).to_array();
    [
        RouteBundle {
            center_theta: west,
            spread: 0.50,
            count: 7,
            lane_range: (280.0, 620.0),
            arc_range: (0.25, 0.9),
            // Terminates at the wall base under the mounted wheel — clustered
            // a fixed 160/30 units short of the panel itself
            // (`palette::WALL_APOTHEM`), so the gap from the wall stays the
            // same size the 2026-07-10 evening apothem bump moved the wall.
            pad_range: (palette::WALL_APOTHEM - 160.0, palette::WALL_APOTHEM - 30.0),
            hue: trace_crimson,
            brightness_range: (0.7, 1.15),
        },
        RouteBundle {
            center_theta: east,
            spread: 0.50,
            count: 7,
            lane_range: (300.0, 650.0),
            arc_range: (0.25, 0.9),
            // Stretched ×13/11 from (450, 900) with `FLOOR_RADIUS` (1100 →
            // 1300, the same evening apothem bump) — this bundle doesn't
            // terminate AT a wall (Tracks keeps its floor marker, not a
            // wall-mounted instrument), so it scales with the floor's own
            // growth rather than `WALL_APOTHEM`'s fixed gap.
            pad_range: (532.0, 1064.0),
            hue: trace_crimson,
            brightness_range: (0.7, 1.15),
        },
        RouteBundle {
            center_theta: north,
            spread: 0.42,
            count: 5,
            lane_range: (270.0, 560.0),
            arc_range: (0.2, 0.75),
            // Stretched ×13/11 from (400, 820) — see the E bundle's comment.
            pad_range: (473.0, 969.0),
            hue: trace_green,
            brightness_range: (0.75, 1.15),
        },
        RouteBundle {
            center_theta: north,
            spread: 0.55,
            count: 4,
            lane_range: (340.0, 660.0),
            arc_range: (0.2, 0.8),
            // Stretched ×13/11 from (460, 850) — see the E bundle's comment.
            pad_range: (544.0, 1005.0),
            hue: trace_cyan,
            brightness_range: (0.7, 1.1),
        },
        RouteBundle {
            center_theta: ne,
            spread: 0.22,
            count: 3,
            // Lane floor ≥ RING_OUTER_R + ROUTE_CHAMFER (248): a lower lane
            // puts the bend's `lane_r − chamfer` point INSIDE the 230
            // departure ring — a backward zig-zag spike. The arc floor keeps
            // one unlucky hash draw from collapsing the stub to a dead-zero
            // span (kaibo review, 2026-07-10).
            lane_range: (252.0, 320.0),
            arc_range: (0.02, 0.12),
            pad_range: (300.0, 420.0),
            hue: violet_glass,
            brightness_range: (0.8, 1.2),
        },
        RouteBundle {
            center_theta: se,
            spread: 0.20,
            count: 2,
            // Lane floor ≥ RING_OUTER_R + ROUTE_CHAMFER (248): a lower lane
            // puts the bend's `lane_r − chamfer` point INSIDE the 230
            // departure ring — a backward zig-zag spike. The arc floor keeps
            // one unlucky hash draw from collapsing the stub to a dead-zero
            // span (kaibo review, 2026-07-10).
            lane_range: (252.0, 320.0),
            arc_range: (0.02, 0.12),
            pad_range: (300.0, 420.0),
            hue: violet_glass,
            brightness_range: (0.8, 1.2),
        },
        RouteBundle {
            center_theta: sw,
            spread: 0.20,
            count: 2,
            // Lane floor ≥ RING_OUTER_R + ROUTE_CHAMFER (248): a lower lane
            // puts the bend's `lane_r − chamfer` point INSIDE the 230
            // departure ring — a backward zig-zag spike. The arc floor keeps
            // one unlucky hash draw from collapsing the stub to a dead-zero
            // span (kaibo review, 2026-07-10).
            lane_range: (252.0, 320.0),
            arc_range: (0.02, 0.12),
            pad_range: (300.0, 420.0),
            hue: violet_glass,
            brightness_range: (0.8, 1.2),
        },
        RouteBundle {
            center_theta: nw,
            spread: 0.22,
            count: 3,
            // Lane floor ≥ RING_OUTER_R + ROUTE_CHAMFER (248): a lower lane
            // puts the bend's `lane_r − chamfer` point INSIDE the 230
            // departure ring — a backward zig-zag spike. The arc floor keeps
            // one unlucky hash draw from collapsing the stub to a dead-zero
            // span (kaibo review, 2026-07-10).
            lane_range: (252.0, 320.0),
            arc_range: (0.02, 0.12),
            pad_range: (300.0, 420.0),
            hue: violet_glass,
            brightness_range: (0.8, 1.2),
        },
        RouteBundle {
            center_theta: south,
            spread: 0.9,
            count: 2,
            lane_range: (300.0, 500.0),
            arc_range: (0.15, 0.4),
            // Stretched ×13/11 from (380, 600) — see the E bundle's comment.
            pad_range: (449.0, 709.0),
            hue: trace_gold,
            brightness_range: (0.8, 1.0),
        },
    ]
}

/// The floor disc (gradient), the inscribed gold double-ring routes depart
/// from, and every circuit-board route + terminal pad (`shell.md`, "the
/// floor is the wiring"). The disc, ring, and traces are **the chamber** —
/// no [`RoomDistraction`] — so they stay lit through a station dive.
fn spawn_floor(
    commands: &mut Commands,
    root: Entity,
    palette: &ScenePalette,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
    glow_mats: &mut Assets<TraceGlowMaterial>,
) {
    // Floor disc — a radial vertex-colour gradient (warm charcoal pooling
    // under the table's glow, fading to near-black at the rim) replaces the
    // old flat fill; `disc_vertices` matches `Circle`'s mesh convention (XY
    // plane, +Z normal), so the same tip-to-XZ rotation applies.
    commands.spawn((
        Mesh3d(meshes.add(floor_mesh(FLOOR_RADIUS, FLOOR_RINGS, FLOOR_SEGMENTS))),
        MeshMaterial3d(mats.add(StandardMaterial {
            base_color: Color::WHITE, // vertex colours carry the gradient
            unlit: true,
            ..default()
        })),
        Transform::from_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
        Visibility::Inherited,
        Name::new("RoomFloor"),
        ChildOf(root),
    ));

    // Inscribed gold ring (concept 06's gold circle band): a thin annulus at
    // RING_OUTER_R; routes depart from it. Mode 1 (breathing) — Annulus
    // ignores uv, so mode 0 would be safe too, but the ring reads as ONE calm
    // architectural body, not wiring, so it breathes
    // (`ScenePalette::trough_subtle`). Its hue used to be its own
    // near-duplicate gold const (`RING_GOLD_HUE`) — collapsed onto
    // `ScenePalette::gold` (the sanctioned value change; see this module's
    // header note on `route_bundles`).
    let gold = ScenePalette::vec3(palette.gold).to_array();
    let ring_mat = glow_mats.add(TraceGlowMaterial {
        color: crest_color(gold, palette.crest),
        params: Vec4::new(0.0, GOLD_GLOW_RATE, palette.trough_subtle, 1.0),
    });
    commands.spawn((
        Mesh3d(meshes.add(Annulus::new(
            RING_OUTER_R - RING_BAND_WIDTH * 0.5,
            RING_OUTER_R + RING_BAND_WIDTH * 0.5,
        ))),
        MeshMaterial3d(ring_mat),
        Transform::from_xyz(0.0, TRACE_Y, 0.0)
            .with_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
        Visibility::Inherited,
        Name::new("ConsoleRingInlay"),
        ChildOf(root),
    ));

    // Circuit-board routes: each bundle expands to several routes; every
    // route gets a traveling-wave ribbon (mode 0) + a breathing terminal pad
    // (mode 1) sharing its crest colour and its glow_phase01 hash draw ("the
    // same hash stream" — the pad's breath and the trace's wave start from
    // the same per-route random offset).
    for (bi, bundle) in route_bundles(palette).iter().enumerate() {
        let seed_base = bi as u32 * 10_007;
        for route in bearing::expand_bundle(
            bundle,
            RING_OUTER_R,
            ROUTE_CHAMFER,
            ROUTE_ARC_SEGMENTS,
            TRACE_Y,
            ROUTE_PAD_RADIUS_RANGE,
            seed_base,
        ) {
            debug_assert!(
                route.points.iter().all(|p| (p[0] * p[0] + p[2] * p[2]).sqrt() > KEEPOUT_RADIUS),
                "every generated route must clear the console keep-out ({KEEPOUT_RADIUS})"
            );
            let identity = [
                route.hue[0] * route.brightness_scale,
                route.hue[1] * route.brightness_scale,
                route.hue[2] * route.brightness_scale,
            ];
            let crest = crest_color(identity, palette.crest);
            debug_assert!(
                crest.x.max(crest.y).max(crest.z) <= palette.crest + 1e-4,
                "floor trace crest must stay capped at ScenePalette::crest: {crest:?}"
            );
            debug_assert!(
                palette.trough_wiring * palette.crest < 1.0,
                "floor trace trough must stay LDR even against the crest cap"
            );

            // Period jitter and phase both ride the route's one spare hash
            // draw (`glow_phase01` — see `bearing::expand_bundle`'s doc on
            // why there's only one to spare); this correlates a route's
            // phase with its period, a minor, deliberately-accepted trade
            // for staying inside the existing seed stride.
            let period =
                bearing::lerp(TRACE_GLOW_PERIOD_RANGE.0, TRACE_GLOW_PERIOD_RANGE.1, route.glow_phase01);
            let trace_mat = glow_mats.add(TraceGlowMaterial {
                color: crest,
                params: Vec4::new(route.glow_phase01, 1.0 / period, palette.trough_wiring, 0.0),
            });
            let mesh = meshes.add(ribbon_mesh(&route.points, TRACE_WIDTH * route.width_scale));
            commands.spawn((
                Mesh3d(mesh),
                MeshMaterial3d(trace_mat),
                Transform::default(),
                Visibility::Inherited,
                Name::new("FloorTrace"),
                ChildOf(root),
            ));

            debug_assert!(
                palette.trough_pads * palette.crest < 1.0,
                "floor trace pad trough must stay LDR even against the crest cap"
            );
            let pad_mat = glow_mats.add(TraceGlowMaterial {
                color: crest,
                params: Vec4::new(
                    route.glow_phase01 * std::f32::consts::TAU,
                    PAD_GLOW_RATE,
                    palette.trough_pads,
                    1.0,
                ),
            });
            commands.spawn((
                Mesh3d(meshes.add(Circle::new(route.pad_radius))),
                MeshMaterial3d(pad_mat),
                Transform::from_translation(Vec3::new(
                    route.pad_pos[0],
                    TRACE_Y + 0.15,
                    route.pad_pos[2],
                ))
                .with_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
                Visibility::Inherited,
                Name::new("FloorTracePad"),
                ChildOf(root),
            ));
        }
    }
}

/// The well table the time well's rings hover above — heavy furniture, not
/// hologram (Amy: "MORE SOLIDNESS"): a chunky dark tabletop, a gold rim torus
/// at its edge, a narrower pedestal, and a wide low plinth grounding it to the
/// floor. **No [`RoomDistraction`]** (Slice C, `lovely-swimming-prism.md`,
/// step 9 — a deliberate change from the slice-A placeholder era, when this
/// was just the console rings' stand, tagged as chrome alongside them): the
/// well now stands ON this table and diving IS diving into the well, so the
/// table it's physically standing on has to stay visible through the dive
/// too, the same way the wheel's own wall panel stays lit through ITS dive.
fn spawn_table(
    commands: &mut Commands,
    root: Entity,
    palette: &ScenePalette,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
) {
    let table_mat = mats.add(unlit(Color::LinearRgba(palette.table)));
    let gold_mat = mats.add(unlit(lin_scaled(palette.gold, palette.trim)));

    // Plinth — wide and low, grounding the table to the floor.
    commands.spawn((
        Mesh3d(meshes.add(Cylinder::new(TABLE_PLINTH_RADIUS, TABLE_PLINTH_HEIGHT))),
        MeshMaterial3d(table_mat.clone()),
        Transform::from_xyz(0.0, TABLE_PLINTH_HEIGHT * 0.5, 0.0),
        Visibility::Inherited,
        Name::new("ConsoleTablePlinth"),
        ChildOf(root),
    ));

    // Pedestal — narrower, rising from the plinth to the tabletop's underside.
    let pedestal_top = TABLE_TOP_Y - TABLE_THICKNESS * 0.5;
    let pedestal_height = pedestal_top - TABLE_PLINTH_HEIGHT;
    commands.spawn((
        Mesh3d(meshes.add(Cylinder::new(TABLE_PEDESTAL_RADIUS, pedestal_height))),
        MeshMaterial3d(table_mat.clone()),
        Transform::from_xyz(0.0, TABLE_PLINTH_HEIGHT + pedestal_height * 0.5, 0.0),
        Visibility::Inherited,
        Name::new("ConsoleTablePedestal"),
        ChildOf(root),
    ));

    // Tabletop — the chunky slab the console rings float above.
    commands.spawn((
        Mesh3d(meshes.add(Cylinder::new(TABLE_RADIUS, TABLE_THICKNESS))),
        MeshMaterial3d(table_mat),
        Transform::from_xyz(0.0, TABLE_TOP_Y - TABLE_THICKNESS * 0.5, 0.0),
        Visibility::Inherited,
        Name::new("ConsoleTableTop"),
        ChildOf(root),
    ));

    // Gold rim torus at the tabletop's edge.
    commands.spawn((
        Mesh3d(meshes.add(Torus { minor_radius: TABLE_RIM_MINOR, major_radius: TABLE_RADIUS })),
        MeshMaterial3d(gold_mat),
        Transform::from_xyz(0.0, TABLE_TOP_Y, 0.0),
        Visibility::Inherited,
        Name::new("ConsoleTableRim"),
        ChildOf(root),
    ));
}

/// The octagon wall shell: eight single-sided, inward-facing panels enclosing
/// the room (`bearing`'s own module comment has the culling mechanics), their
/// corner mullions, a hue-coded neon edge-trim per panel, and — on the four
/// diagonals — the violet information threads that used to stand as
/// free-floating radiators (now the panel's own content). Every part here is
/// the CHAMBER (no [`RoomDistraction`]): it stays lit through a station dive
/// the same as the floor and the dome.
fn spawn_walls(
    commands: &mut Commands,
    root: Entity,
    palette: &ScenePalette,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
    glow_mats: &mut Assets<TraceGlowMaterial>,
) {
    let wall_apothem = crate::view::palette::WALL_APOTHEM;
    let panel_width = bearing::octagon_panel_width(wall_apothem) - WALL_PANEL_GAP;

    let base_mesh = meshes.add(Rectangle::new(panel_width, WALL_HEIGHT));
    let base_mat = mats.add(unlit(Color::LinearRgba(palette.wall_base)));
    // `glow_quad_mesh` (not a plain `Rectangle`) so `uv.x` tracks each trim
    // strip's own REAL length: the top/bottom strips are wide-and-short, the
    // left/right strips are narrow-and-tall, and `TraceGlowMaterial`'s
    // mode-0 wave needs `uv.x` to run along whichever that is (`glow_quad_mesh`'s
    // own doc has the why).
    let h_trim_mesh = meshes.add(glow_quad_mesh(panel_width, WALL_TRIM_THICKNESS));
    let v_trim_mesh = meshes.add(glow_quad_mesh(WALL_TRIM_THICKNESS, WALL_HEIGHT));
    let mullion_mesh = meshes.add(Rectangle::new(WALL_MULLION_WIDTH, WALL_HEIGHT));
    let mullion_mat = mats.add(unlit(Color::LinearRgba(palette.wall_mullion)));

    // Identity hue per bearing (read straight off `wall_placements`, so a
    // re-tuned marker hue can't drift out of sync with its wall) plus one
    // shared violet for every diagonal face — fed to `crest_color` per panel
    // below (each panel needs its OWN `TraceGlowMaterial` instance for its
    // own phase, so hue lookup stays a closure rather than pre-built handles).
    let placements = bearing::wall_placements();
    let hue_for = |b: Bearing| -> [f32; 3] {
        placements
            .iter()
            .find(|wp| wp.bearing == b)
            .map(|wp| wp.hue)
            .expect("wall_placements always covers all four cardinal bearings")
    };

    for (i, panel) in bearing::octagon_panels(wall_apothem).iter().enumerate() {
        let pos = Vec3::new(panel.center[0], WALL_HEIGHT * 0.5, panel.center[2]);
        // Inward-facing single-sided quad: aim local −Z further out along the
        // same ray so local +Z — the mesh's front normal, and where the trim/
        // thread proud-offsets below land — points at the console (the
        // `spawn_radiators` trick this replaces, now the wall's own).
        let outward = Vec3::new(pos.x * 2.0, pos.y, pos.z * 2.0);
        let panel_tf = Transform::from_translation(pos).looking_at(outward, Vec3::Y);

        commands.spawn((
            Mesh3d(base_mesh.clone()),
            MeshMaterial3d(base_mat.clone()),
            panel_tf,
            Visibility::Inherited,
            Name::new(format!("WallPanel{i}")),
            ChildOf(root),
        ));

        let identity_hue = match panel.bearing {
            Some(b @ (Bearing::North | Bearing::East | Bearing::South | Bearing::West)) => {
                hue_for(b)
            }
            Some(Bearing::Center) => unreachable!("octagon panels never carry the center bearing"),
            None => ScenePalette::vec3(palette.violet_thread).to_array(),
        };
        // Per-panel material (not a shared handle): each panel needs its OWN
        // phase (`bearing::hash01`, namespaced well away from the thread
        // jitter seeds below — `i*90_001` vs. the threads' `i*251+j*17` — so
        // neither draw can replay the other's) so the eight panels shimmer
        // asynchronously rather than in lockstep.
        let phase = bearing::hash01(i as u32 * 90_001 + 4242);
        let trim_mat = glow_mats.add(TraceGlowMaterial {
            color: crest_color(identity_hue, palette.crest),
            params: Vec4::new(phase, 1.0 / WALL_TRIM_GLOW_PERIOD, palette.trough_wall_trim, 0.0),
        });
        let half_h = WALL_HEIGHT * 0.5 - WALL_TRIM_THICKNESS * 0.5;
        let half_w = panel_width * 0.5 - WALL_TRIM_THICKNESS * 0.5;
        for (edge, mesh, local) in [
            ("Top", h_trim_mesh.clone(), Vec3::new(0.0, half_h, WALL_TRIM_PROUD)),
            ("Bottom", h_trim_mesh.clone(), Vec3::new(0.0, -half_h, WALL_TRIM_PROUD)),
            ("Left", v_trim_mesh.clone(), Vec3::new(-half_w, 0.0, WALL_TRIM_PROUD)),
            ("Right", v_trim_mesh.clone(), Vec3::new(half_w, 0.0, WALL_TRIM_PROUD)),
        ] {
            commands.spawn((
                Mesh3d(mesh),
                MeshMaterial3d(trim_mat.clone()),
                Transform::from_translation(panel_tf.transform_point(local))
                    .with_rotation(panel_tf.rotation),
                Visibility::Inherited,
                Name::new(format!("WallPanel{i}Trim{edge}")),
                ChildOf(root),
            ));
        }

        // Diagonal faces only: the migrated violet content threads (the old
        // free-floating radiators' foreground detail).
        if panel.bearing.is_none() {
            for j in 0..WALL_THREAD_COUNT {
                let seed = i as u32 * 251 + j as u32 * 17;
                let t = if WALL_THREAD_COUNT > 1 {
                    j as f32 / (WALL_THREAD_COUNT - 1) as f32
                } else {
                    0.5
                };
                let x = (t - 0.5) * panel_width * 0.82;
                let h = bearing::lerp(
                    WALL_HEIGHT * WALL_THREAD_HEIGHT_RANGE.0,
                    WALL_HEIGHT * WALL_THREAD_HEIGHT_RANGE.1,
                    bearing::hash01(seed),
                );
                let brightness = bearing::lerp(
                    WALL_THREAD_BRIGHTNESS_RANGE.0,
                    WALL_THREAD_BRIGHTNESS_RANGE.1,
                    bearing::hash01(seed + 1),
                );
                let local = Vec3::new(x, 0.0, WALL_THREAD_PROUD);
                commands.spawn((
                    Mesh3d(meshes.add(Rectangle::new(WALL_THREAD_WIDTH, h))),
                    MeshMaterial3d(mats.add(unlit(lin_scaled(palette.violet_thread, brightness)))),
                    Transform::from_translation(panel_tf.transform_point(local))
                        .with_rotation(panel_tf.rotation),
                    Visibility::Inherited,
                    Name::new(format!("WallPanel{i}Thread{j}")),
                    ChildOf(root),
                ));
            }
        }
    }

    for (i, (pos, _theta)) in bearing::octagon_corners(wall_apothem).iter().enumerate() {
        let center = Vec3::new(pos[0], WALL_HEIGHT * 0.5, pos[2]);
        let outward = Vec3::new(center.x * 2.0, center.y, center.z * 2.0);
        let tf = Transform::from_translation(center).looking_at(outward, Vec3::Y);
        commands.spawn((
            Mesh3d(mullion_mesh.clone()),
            MeshMaterial3d(mullion_mat.clone()),
            tf,
            Visibility::Inherited,
            Name::new(format!("WallMullion{i}")),
            ChildOf(root),
        ));
    }
}

/// Whether a wall bearing's pylon earns a gold cap slab — every built station
/// does; the reserved South marker stays humble (a plinth only): `shell.md`'s
/// "future MCP broker switchboard" placeholder shouldn't out-dress the
/// stations that actually exist yet. Pure — no Bevy types — so the gating is
/// unit-testable without spawning anything.
fn wants_gold_cap(wp: &bearing::WallPlacement) -> bool {
    wp.station.is_some()
}

/// Pylon furniture: a wide low plinth grounding every marker to the floor,
/// and a gold cap slab on top of every built station's pylon
/// ([`wants_gold_cap`] gates the reserved South stub out). Skips the
/// furnished W bearing entirely ([`bearing::station_is_room_furniture`]) —
/// the wheel mounts on the wall panel itself, no pylon/plinth/cap of its own.
fn spawn_pylons(
    commands: &mut Commands,
    root: Entity,
    palette: &ScenePalette,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
) {
    let plinth_mesh =
        meshes.add(Cuboid::new(PYLON_PLINTH_WIDTH, PYLON_PLINTH_HEIGHT, PYLON_PLINTH_WIDTH));
    let plinth_mat = mats.add(unlit(Color::LinearRgba(palette.table)));
    let cap_mesh = meshes.add(Cuboid::new(PYLON_CAP_WIDTH, PYLON_CAP_HEIGHT, PYLON_CAP_WIDTH));
    let cap_mat = mats.add(unlit(lin_scaled(palette.gold, palette.trim)));

    for wp in bearing::wall_placements() {
        if wp.station.is_some_and(bearing::station_is_room_furniture) {
            continue;
        }

        let marker_h = if wp.station.is_some() { MARKER_HEIGHT } else { MARKER_HEIGHT_RESERVED };
        let base = Vec3::new(wp.dir[0] * ROOM_RADIUS, 0.0, wp.dir[2] * ROOM_RADIUS);

        commands.spawn((
            Mesh3d(plinth_mesh.clone()),
            MeshMaterial3d(plinth_mat.clone()),
            Transform::from_translation(base + Vec3::Y * (PYLON_PLINTH_HEIGHT * 0.5)),
            RoomDistraction,
            Visibility::Inherited,
            Name::new(format!("BearingPlinth-{:?}", wp.bearing)),
            ChildOf(root),
        ));

        if wants_gold_cap(&wp) {
            commands.spawn((
                Mesh3d(cap_mesh.clone()),
                MeshMaterial3d(cap_mat.clone()),
                // Rides the plinth-seated marker (see the marker spawn in
                // `enter_room`): plinth + pylon + cap stack cleanly.
                Transform::from_translation(
                    base + Vec3::Y * (PYLON_PLINTH_HEIGHT + marker_h + PYLON_CAP_HEIGHT * 0.5),
                ),
                RoomDistraction,
                Visibility::Inherited,
                Name::new(format!("BearingCap-{:?}", wp.bearing)),
                ChildOf(root),
            ));
        }
    }
}

// ── Systems ───────────────────────────────────────────────────────────────────

/// Whether Enter/Down on a focused station fullscreens the camera onto it
/// (`RoomState::zoomed`) rather than diving to a dedicated `Screen` — the
/// wheel (2026-07-10 evening, the fullscreen-panel pivot: "diving IS
/// fullscreening a panel" for a *bounded* station) and, since the
/// time-well/room integration plan's Slice C, the time well too (its own
/// "diving IS the room's shot table zooming into the well's mouth" pose,
/// `view/room/shot.rs::RoomShot::WellOverview`) — N stays a future
/// dive-THROUGH door onto an unbounded world, not a panel/pose to fill the
/// frame with. A future in-room station with content of its own — the
/// highway, the archway — adds itself here as that content lands. Pure — no
/// Bevy types — unit-tested like `bearing::station_is_room_furniture`, which
/// this pairs with (same stations, same reasoning: each IS its own furniture
/// AND its own zoom target).
fn station_is_zoomable(station: Station) -> bool {
    matches!(station, Station::PatchBay | Station::TimeWell)
}

/// Room keys: Left/Right cycle the carousel, Enter/Down dives, Esc drops to
/// the conversation (the room is the top level) — the nav contract is frozen,
/// unchanged from the blockout. Since Slice C every zoomable station
/// (including `TimeWell` now) shares ONE dive mechanism: Enter/Down sets
/// `RoomState::zoomed` — a camera pose, not a screen — and arms whatever
/// per-station text/dive-state that station needs (`pb_state.arm_text()` for
/// the wheel, `time_well::scene::arm_dive` for the well).
///
/// While zoomed, this system steps back almost entirely: Left/Right and
/// Esc/Up belong to the zoomed station's OWN keyboard system now (patch_bay's
/// `patch_bay_keyboard` gated on `patch_bay_zoomed`; the well's
/// `time_well::scene::well_keyboard` gated on `time_well::scene::well_zoomed`)
/// — "the zoomed station's own keys own them," the same reasoning
/// `bearing::station_is_room_furniture` already applies to their geometry.
/// The one thing this system still owns while zoomed is nothing at all; it
/// simply returns, so there's no double-handling between the systems.
///
/// **Must run before `well_keyboard`/`patch_bay_keyboard` in the same tick**
/// (a kaibo review round, 2026-07-11, hardening what plugin registration
/// order in `main.rs` already relied on implicitly): if a zoomed station's
/// own Esc handler clears `RoomState::zoomed` to `None` BEFORE this system's
/// own early-return check runs, this system would see the just-cleared
/// `zoomed` in the SAME frame and fire ITS OWN Escape-to-Conversation branch
/// too, skipping the room-overview stop entirely. `pub(crate)` so
/// `time_well`/`patch_bay`'s plugins can declare `.after(room_keyboard)`
/// explicitly instead of leaning on `main.rs`'s plugin-addition order (which
/// is real today but easy to silently break by reordering plugins later).
pub(crate) fn room_keyboard(
    keys: Res<ButtonInput<KeyCode>>,
    mut room: ResMut<RoomState>,
    mut pb_state: ResMut<patch_bay::PatchBayState>,
    mut well_state: ResMut<crate::view::time_well::scene::TimeWellState>,
    mut next: ResMut<NextState<Screen>>,
) {
    if room.zoomed.is_some() {
        return;
    }

    if keys.just_pressed(KeyCode::ArrowRight) || keys.just_pressed(KeyCode::Tab) {
        room.carousel.step(1);
    } else if keys.just_pressed(KeyCode::ArrowLeft) {
        room.carousel.step(-1);
    }

    if keys.just_pressed(KeyCode::Enter) || keys.just_pressed(KeyCode::ArrowDown) {
        let station = room.carousel.focused_station();
        if station_is_zoomable(station) {
            room.zoomed = Some(station);
            // Arm the newly-zoomed station's own per-dive state — the old
            // `enter_patch_bay`'s job for the wheel, moved to the zoom-in site
            // since there's no more `OnEnter(Screen::PatchBay)` to hang it on;
            // the well's `arm_dive` (Slice C) follows the same shape.
            match station {
                Station::PatchBay => pb_state.arm_text(),
                Station::TimeWell => crate::view::time_well::scene::arm_dive(&mut well_state),
                _ => {}
            }
        }
        // Unbuilt, non-zoomable stations: stay put (the dimmed plate says why).
        return;
    }

    if keys.just_pressed(KeyCode::Escape) {
        next.set(Screen::Conversation);
    }
}

/// Fill the nameplates whenever [`RoomState::plates_dirty`] is armed (the same
/// async-font gate as the well's label builders). A per-entry dirty flag, not
/// a process-lifetime latch — see the comment on `plates_dirty` for why a
/// `Local<bool>` "done, never again" latch is wrong here: `StationPlate`
/// entities die with `RoomRoot` on every `exit_room`, but a `Local` survives
/// the whole app run. `enter_room`'s `arm_on_enter` re-arms the flag on every
/// entry; this is the one system that clears it, and only once it actually
/// commits glyphs.
fn room_plate_text(
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    mut room: ResMut<RoomState>,
    mut plates: Query<(&StationPlate, &mut MsdfBlockGlyphs)>,
) {
    if !room.plates_dirty {
        return;
    }
    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };
    let Some(atlas) = atlas.as_deref_mut() else {
        return;
    };
    let mut any = false;
    for (plate, mut msdf) in plates.iter_mut() {
        let station = Station::ALL[plate.0];
        let glyphs = layout_plate_text(station.label(), font, atlas, &mut font_data_map);
        commit_panel_glyphs(&mut msdf, glyphs);
        any = true;
    }
    // Plates spawn via Commands the same frame `enter_room` runs, and
    // OnEnter's commands are applied before Update — so querying them here on
    // entry is normally fine. Still: only clear the arm once we actually
    // filled something, so a scheduling surprise can't eat it and leave the
    // plates blank with nothing left to re-trigger a fill.
    if any {
        room.plates_dirty = false;
    }
}

/// Focus presentation for the plates: brighten + grow the focused plate,
/// brass-frame it; unbuilt stations stay dim even focused. PatchBay spawns no
/// plate at all now (the wheel is the station) — the query below simply never
/// yields one for it, so focusing PatchBay brightens nothing here and that's
/// fine: the camera's approach on the mounted wheel is the feedback instead.
fn room_focus_visuals(
    room: Res<RoomState>,
    mut materials: ResMut<Assets<WellCardMaterial>>,
    mut plates: Query<(&StationPlate, &MeshMaterial3d<WellCardMaterial>, &mut Transform)>,
) {
    for (plate, mat_handle, mut tf) in plates.iter_mut() {
        let station = Station::ALL[plate.0];
        let focused = plate.0 == room.carousel.focused;
        let dim = if focused && station.built() {
            PLATE_DIM_FOCUSED
        } else if station.built() {
            PLATE_DIM_IDLE
        } else if focused {
            (PLATE_DIM_UNBUILT + 0.15).min(PLATE_DIM_IDLE)
        } else {
            PLATE_DIM_UNBUILT
        };
        let scale = if focused { PLATE_SCALE_FOCUSED } else { 1.0 };
        if tf.scale.x != scale {
            tf.scale = Vec3::splat(scale);
        }
        // Guarded write: only dirty the material when focus actually flips, so a
        // settled plate stops re-extracting (the well's asset discipline).
        let border = if focused { PLATE_BORDER_FOCUSED } else { Vec4::ZERO };
        if materials.get(&mat_handle.0).is_some_and(|m| m.dim.x != dim || m.border != border) {
            if let Some(mat) = materials.get_mut(&mat_handle.0) {
                mat.dim.x = dim;
                mat.border = border;
            }
        }
    }
}

/// Ease the shell camera toward its target pose — travel by intent, the same
/// exponentially-smoothed tween the well used to run on its own separate
/// camera (`time_well::scene::ease_camera_to_focused_ring`, deleted once the
/// well became room furniture — see `shot::WellOverview`'s doc). Reads the
/// target from [`RoomState::zoomed`] now, not a second `Screen` (2026-07-10
/// evening, the fullscreen-panel pivot): at room scale it faces the focused
/// station's bearing ([`shot::RoomShot::focused`]); zoomed onto a wall
/// station it fills the frame with that station's panel
/// ([`shot::RoomShot::Fullscreen`]); zoomed onto the well (Slice C, no wall
/// bearing to fill a frame with) it dollies onto the well's own focused ring
/// or focus card ([`shot::RoomShot::WellOverview`]). One system, one camera —
/// diving and surfacing are the SAME continuous glide, just retargeted the
/// frame `zoomed` flips (no snap either way). The "what pose do we want"
/// question is [`shot::resolve`]'s job; this system only owns the ease
/// itself.
fn ease_shell_camera(
    time: Res<Time>,
    room: Res<RoomState>,
    well_state: Res<crate::view::time_well::scene::TimeWellState>,
    mut cam: Query<&mut Transform, With<RoomCamera>>,
) {
    let Ok(mut tf) = cam.single_mut() else {
        return;
    };
    let want = match room.zoomed {
        Some(Station::TimeWell) if well_state.hero => shot::RoomShot::WellHero,
        Some(Station::TimeWell) => shot::RoomShot::WellOverview(shot::WellShotInput {
            focused: well_state.focused,
            focused_ring: well_state.focused_ring,
        }),
        Some(station) => shot::RoomShot::Fullscreen(station),
        None => shot::RoomShot::focused(room.carousel.focused_station()),
    };
    let (pos, look) = shot::resolve(want);
    let desired = Transform::from_translation(pos).looking_at(look, Vec3::Y);
    let alpha = 1.0 - (-CAMERA_EASE_RATE * time.delta_secs()).exp();
    tf.translation = tf.translation.lerp(desired.translation, alpha);
    tf.rotation = tf.rotation.slerp(desired.rotation, alpha);
}

/// Fade the room chrome on a zoom: hide the [`RoomDistraction`] chrome (bearing
/// pylons, nameplates, console emblem, radiators) while [`RoomState::zoomed`]
/// is `Some` so the fullscreened station owns the eye, and restore it at room
/// scale. The floor, its traces, the vault dome, and the zoomed station itself
/// stay — they are the chamber, not distractions. One mechanism (Visibility),
/// change-guarded so settled chrome never re-dirties (`docs/scenes/shell.md`
/// — the zoomed view earns its focus by hiding distractions and showing the
/// labels, same effect as the old dive, now keyed on the resource instead of
/// a second `Screen`).
fn apply_room_dive_visibility(
    room: Res<RoomState>,
    mut chrome: Query<&mut Visibility, With<RoomDistraction>>,
) {
    let want = if room.zoomed.is_some() { Visibility::Hidden } else { Visibility::Inherited };
    for mut vis in chrome.iter_mut() {
        if *vis != want {
            *vis = want;
        }
    }
}

/// Ingest the kernel-wide event stream into per-bearing activity, **ungated**
/// (every screen) so the room opens warm. The freshest source, no re-wire:
/// beat syncs warm the tracks (E) bearing, block chatter warms the console.
fn ingest_room_activity(
    mut events: MessageReader<ServerEventMessage>,
    mut room_activity: ResMut<BearingActivity>,
    time: Res<Time>,
) {
    for ServerEventMessage(ev) in events.read() {
        if let Some((b, w)) = activity::event_bearing(ev) {
            room_activity.record(b, w);
        }
    }
    room_activity.tick(time.delta_secs());
}

/// Push ambient telemetry into the wall-bearing markers as light: the tracks
/// (E) marker breathes with the well's beat phasor (HDR pulse decaying to
/// LDR), and the focused station's marker takes a steady lift. Change-guarded
/// + quantized so a settled marker never touches `Assets<StandardMaterial>`
/// (the well's `sync_card_live_uniforms` discipline).
///
/// The console-emblem glow branch this used to carry (`ConsoleEmblem`,
/// `CONSOLE_CHATTER_GAIN`) is gone with the slice-A placeholder it lit (Slice
/// C, `lovely-swimming-prism.md`) — the real time well now stands at the
/// console bearing and owns its own chatter/energy glow entirely through
/// `time_well::activity::RingActivity` (fed by the well's own
/// `accumulate_ring_activity`, not this system). `BearingActivity(Center)`
/// used to keep accumulating chatter from `ingest_room_activity` with no
/// reader left for it — closed in the freeze-fix slice (2026-07-11) by
/// dropping the Center mapping from `activity::event_bearing` entirely
/// (`Bearing::Center` itself stays for room geometry, just unfed).
fn sync_room_glow(
    room_activity: Res<BearingActivity>,
    beats: Res<WellBeats>,
    room: Res<RoomState>,
    palette: Res<ScenePalette>,
    mut mats: ResMut<Assets<StandardMaterial>>,
    markers: Query<(&BearingMarker, &MeshMaterial3d<StandardMaterial>)>,
) {
    let now = Instant::now();
    let beat = beats.global_envelope(now);
    let focused = room.carousel.focused_station();

    for (marker, handle) in markers.iter() {
        let mut lift = 0.0;
        if marker.bearing == Bearing::East {
            lift += beat * palette.gain_beat
                + room_activity.normalized(Bearing::East) * palette.gain_active;
        }
        if marker.station == Some(focused) {
            lift += palette.gain_focus_lift;
        }
        let brightness = quantize(palette.marker + lift);
        set_glow(&mut mats, &handle.0, marker.hue * brightness);
    }
}

// ── Material + colour helpers ──────────────────────────────────────────────────

/// An unlit [`StandardMaterial`] carrying its brightness in `base_color` — the
/// room's one emission channel (HDR blooms, LDR reads crisp).
fn unlit(base_color: Color) -> StandardMaterial {
    StandardMaterial { base_color, unlit: true, ..default() }
}

/// A linear-rgb [`Color`] from a [`Vec3`].
fn lin_v(v: Vec3) -> Color {
    Color::LinearRgba(LinearRgba::rgb(v.x, v.y, v.z))
}

/// Renormalize an identity hue×brightness colour so its brightest channel
/// lands exactly at `crest` — every [`TraceGlowMaterial`] element's crest
/// shares the same ceiling ([`ScenePalette::crest`]) regardless of how bright
/// its raw identity colour happens to be; the element's `trough` (baked into
/// `params.z` at the call site) then sets how far the resting brightness
/// sits below it. Degenerate all-zero input maps to itself (a black trace
/// stays black — `scale` would otherwise divide by zero). `.w` is unused
/// (every `TraceGlowMaterial` element is opaque).
///
/// `pub(crate)`: also the crest-normalization the time well's lineage drapes
/// share (`time_well::drape::lineage_drape_material`) — same
/// `TraceGlowMaterial` binding idiom, one normalization helper.
pub(crate) fn crest_color(identity: [f32; 3], crest: f32) -> Vec4 {
    let max_c = identity[0].max(identity[1]).max(identity[2]);
    let scale = if max_c > 1e-6 { crest / max_c } else { 0.0 };
    Vec4::new(identity[0] * scale, identity[1] * scale, identity[2] * scale, 0.0)
}

/// Snap a glow value to the [`GLOW_STEP`] grid so a settled marker stops
/// touching `Assets<StandardMaterial>`.
fn quantize(v: f32) -> f32 {
    (v / GLOW_STEP).round() * GLOW_STEP
}

/// Write a material's `base_color` only when it actually changes (read via the
/// non-dirtying `get`, reach for `get_mut` on change) — the well's per-frame
/// asset-write discipline.
fn set_glow(mats: &mut Assets<StandardMaterial>, handle: &Handle<StandardMaterial>, target: Vec3) {
    let Some(cur) = mats.get(handle).map(|m| m.base_color.to_linear()) else {
        return;
    };
    if (cur.red - target.x).abs() > 1e-4
        || (cur.green - target.y).abs() > 1e-4
        || (cur.blue - target.z).abs() > 1e-4
    {
        if let Some(m) = mats.get_mut(handle) {
            m.base_color = lin_v(target);
        }
    }
}

// ── Procedural meshes ──────────────────────────────────────────────────────────

/// A flat floor ribbon (up-normal) along `points`, `width` across — a trace
/// channel. Vertex math lives in [`bearing::ribbon_vertices`] (unit-tested);
/// this wraps it into a `Mesh` with up-normals and UVs (`uv.x` the
/// cumulative-arclength fraction along the route, which
/// [`TraceGlowMaterial`]'s mode-0 traveling wave rides).
fn ribbon_mesh(points: &[[f32; 3]], width: f32) -> Mesh {
    use bevy::asset::RenderAssetUsages;
    use bevy::mesh::{Indices, PrimitiveTopology};

    let (positions, uvs, indices) = bearing::ribbon_vertices(points, width);
    let normals = vec![[0.0, 1.0, 0.0]; positions.len()];
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
        .with_inserted_indices(Indices::U32(indices))
}

/// A flat quad (`w`×`h`, centered at origin, XY-plane, +Z normal — Bevy's
/// `Rectangle` mesh convention) whose UV is assigned so `uv.x` always tracks
/// the LONGER of the two dimensions and `uv.y` the shorter — unlike
/// `Rectangle::mesh()`, which always ties `uv.x` to the local X axis
/// regardless of which side is actually longer. [`TraceGlowMaterial`]'s
/// mode-0 traveling wave reads `uv.x` as "along the element's length"; a
/// wall-trim strip is wide-and-short on a panel's top/bottom edge but
/// narrow-and-tall on its left/right edge (`spawn_walls`), and the wave must
/// glide along whichever is the strip's REAL length, not always its local X
/// — a plain `Rectangle` mesh on the tall strips would key `uv.x` off the
/// thin thickness axis, and the wave would read as the whole strip pulsing
/// in unison rather than a crest traveling top-to-bottom.
fn glow_quad_mesh(w: f32, h: f32) -> Mesh {
    use bevy::asset::RenderAssetUsages;
    use bevy::mesh::{Indices, PrimitiveTopology};

    let (hw, hh) = (w * 0.5, h * 0.5);
    let positions = vec![[hw, hh, 0.0], [-hw, hh, 0.0], [-hw, -hh, 0.0], [hw, -hh, 0.0]];
    let normals = vec![[0.0, 0.0, 1.0]; 4];
    let uvs: Vec<[f32; 2]> = if w >= h {
        // `Rectangle::mesh()`'s own corner UVs: u along X, v along Y.
        vec![[1.0, 0.0], [0.0, 0.0], [0.0, 1.0], [1.0, 1.0]]
    } else {
        // Swapped: u along Y (the long side here), v along X.
        vec![[1.0, 0.0], [1.0, 1.0], [0.0, 1.0], [0.0, 0.0]]
    };
    let indices = Indices::U32(vec![0, 1, 2, 0, 2, 3]);
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_indices(indices)
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
}

/// The vault dome: a UV sphere with a per-vertex vertical gradient
/// ([`bearing::dome_color`]). Unlit + vertex-colours → the gradient is the
/// output; the material's `base_color` is left white.
fn dome_mesh(radius: f32) -> Mesh {
    use bevy::mesh::VertexAttributeValues;

    let mut mesh = Sphere::new(radius).mesh().uv(48, 24);
    // Compute the gradient into an owned buffer first, so the immutable
    // position borrow ends before the mutable `insert_attribute`.
    let colors: Option<Vec<[f32; 4]>> =
        if let Some(VertexAttributeValues::Float32x3(positions)) =
            mesh.attribute(Mesh::ATTRIBUTE_POSITION)
        {
            Some(
                positions
                    .iter()
                    // t = 0 at the horizon (y = 0), 1 at the apex: the whole
                    // VISIBLE upper hemisphere spans the gradient. The old
                    // `y/r * 0.5 + 0.5` remap put dome_color's documented
                    // "rim" at the hidden south pole and handed the horizon
                    // only the gradient midpoint — the vault read darker and
                    // more void-like than the tuned RIM intends (kaibo,
                    // 2026-07-10).
                    .map(|p| bearing::dome_color((p[1] / radius).max(0.0)))
                    .collect(),
            )
        } else {
            None
        };
    if let Some(colors) = colors {
        mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    }
    mesh
}

/// The floor disc: [`bearing::disc_vertices`]' geometry (mesh-local XY,
/// `Circle`'s convention — `spawn_floor` rotates it flat), coloured by
/// [`bearing::floor_color`]'s radial gradient — the `dome_mesh`/`dome_color`
/// idiom applied to the floor.
fn floor_mesh(radius: f32, rings: usize, segments: usize) -> Mesh {
    use bevy::asset::RenderAssetUsages;
    use bevy::mesh::{Indices, PrimitiveTopology};

    let (positions, indices) = bearing::disc_vertices(radius, rings, segments);
    let normals = vec![[0.0, 0.0, 1.0]; positions.len()];
    let colors: Vec<[f32; 4]> = positions
        .iter()
        .map(|p| bearing::floor_color((p[0] * p[0] + p[1] * p[1]).sqrt() / radius))
        .collect();
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, colors)
        .with_inserted_indices(Indices::U32(indices))
}

// ── Shared plate-text helper (also used by the patch bay's plates) ────────────

fn plate_brush() -> Brush {
    bevy_color_to_brush(Color::srgba(0.82, 0.88, 0.97, 0.9))
}

/// Single-line MSDF layout for a nameplate-style panel sized
/// [`PLATE_TEX_W`]×[`PLATE_TEX_H`]. The brush goes to BOTH `layout` and
/// `collect_msdf_glyphs`, or the text renders black (`docs/timewell.md`,
/// "Landmines").
pub(crate) fn layout_plate_text(
    text: &str,
    font: &VelloFont,
    atlas: &mut MsdfAtlas,
    font_data_map: &mut FontDataMap,
) -> Vec<PositionedGlyph> {
    if text.is_empty() {
        return Vec::new();
    }
    let brush = plate_brush();
    let layout = font.layout(
        text,
        &VelloTextStyle { font_size: PLATE_FONT_SIZE, line_height: 1.1, ..default() },
        VelloTextAlign::Middle,
        Some(PLATE_TEX_W - 2.0 * PLATE_PAD),
    );
    for line in layout.lines() {
        for item in line.items() {
            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                font_data_map.register(gr.run().font());
            }
        }
    }
    collect_msdf_glyphs(&layout, &[], &brush, (PLATE_PAD as f64, PLATE_PAD as f64), atlas)
}

/// `WellCardMaterial.shape` for a nameplate: texture aspect, soft corner, thin
/// border channel (drawn only when `border` is nonzero).
fn plate_shape() -> Vec4 {
    Vec4::new(PLATE_TEX_W / PLATE_TEX_H, 0.10, 0.05, 0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    // -- RoomState::plates_dirty / arm_on_enter -------------------------

    /// The shape `RoomState` is left in after a visit: the carousel focus
    /// (Resource) survives `exit_room` untouched, but `plates_dirty` has
    /// already been cleared by `room_plate_text` filling that visit's
    /// plates — and nothing has re-armed it for the next entry yet. `zoomed`
    /// is always `None` here: `exit_room` clears it unconditionally on the
    /// way out, so no visit can ever persist a lingering zoom.
    fn persisted_after_a_visit() -> RoomState {
        RoomState {
            carousel: StationCarousel::new(Station::PatchBay),
            zoomed: None,
            plates_dirty: false,
        }
    }

    #[test]
    fn fresh_room_state_starts_with_plates_dirty() {
        // The very first-ever visit also needs its plates filled — there is
        // no prior `arm_on_enter` call to have done it.
        assert!(RoomState::default().plates_dirty);
    }

    #[test]
    fn arm_on_enter_forces_plates_dirty_true() {
        let mut room = persisted_after_a_visit();
        arm_on_enter(&mut room);
        assert!(
            room.plates_dirty,
            "re-entry must force a nameplate relayout even though the plates \
             were already filled once, on a previous visit's now-despawned entities"
        );
    }

    #[test]
    fn arm_on_enter_leaves_the_carousel_focus_untouched() {
        let mut room = persisted_after_a_visit();
        let before = room.carousel.focused;
        arm_on_enter(&mut room);
        assert_eq!(room.carousel.focused, before, "arm_on_enter only touches plates_dirty");
    }

    // -- station_is_zoomable / RoomState::zoomed (2026-07-10 evening,
    //    the fullscreen-panel pivot: `zoomed` replaces `Screen::PatchBay`) --

    /// Expected `station_is_zoomable` result, one row per station — a table
    /// to APPEND a row to (not restructure a boolean expression) as new
    /// stations earn a fullscreen zoom. Restructured 2026-07-11 (time-well/
    /// room integration plan, ahead of the parallel Tracker lane) from a
    /// single `s == Station::PatchBay` comparison specifically because two
    /// concurrent lanes were both about to need to edit that same expression
    /// (TimeWell for this lane, Tracks for the Tracker lane) — a shared
    /// boolean expression is a merge-conflict magnet; a table both lanes add
    /// a line to is not. `Station::ALL.len()` guard below catches a station
    /// added to the enum without a row here.
    const EXPECTED_ZOOMABLE: &[(Station, bool)] = &[
        (Station::TimeWell, true),
        (Station::PatchBay, true),
        (Station::Tracks, false),
        (Station::Vfs, false),
        (Station::Radiators, false),
    ];

    #[test]
    fn zoomable_stations_match_the_expected_table() {
        assert_eq!(
            EXPECTED_ZOOMABLE.len(),
            Station::ALL.len(),
            "every station needs a row in EXPECTED_ZOOMABLE"
        );
        for (s, expected) in EXPECTED_ZOOMABLE {
            assert_eq!(station_is_zoomable(*s), *expected, "{s:?}: zoomable mismatch");
        }
    }

    #[test]
    fn zoom_round_trip_never_touches_the_carousel_or_plates_dirty() {
        // Zooming in/out is a plain `RoomState` field write now — there is no
        // `OnEnter`/`OnExit` hook attached to it at all (unlike the old
        // `Screen::PatchBay` transition), so nothing can possibly despawn or
        // rebuild anything on a zoom toggle. A resource-level check is the
        // whole proof; no Bevy `App` is needed to demonstrate "the room stays
        // alive" when there's no teardown wired to this write in the first
        // place.
        let mut room = persisted_after_a_visit();
        let carousel_before = room.carousel.focused;
        let plates_dirty_before = room.plates_dirty;

        room.zoomed = Some(Station::PatchBay);
        assert_eq!(room.zoomed, Some(Station::PatchBay));
        room.zoomed = None;

        assert_eq!(room.carousel.focused, carousel_before, "zooming never touches the carousel");
        assert_eq!(room.plates_dirty, plates_dirty_before, "zooming never touches plates_dirty");
    }

    // -- fullscreen_pose / desired_camera math moved to `shot.rs`'s own test
    //    module (2026-07-11, Slice A of the time-well/room integration) --

    // -- exit_room lifecycle (2026-07-10 evening: unconditional, no more
    //    `Screen::PatchBay` branch to get wrong) --

    fn zoom_lifecycle_app() -> App {
        let mut app = App::new();
        app.add_plugins(bevy::state::app::StatesPlugin)
            .insert_resource(crate::ui::theme::Theme::default())
            .init_resource::<RoomState>()
            .init_state::<Screen>()
            .add_systems(OnExit(Screen::Room), exit_room);
        app
    }

    fn set_screen(app: &mut App, s: Screen) {
        app.world_mut().resource_mut::<NextState<Screen>>().set(s);
        app.update();
    }

    fn room_root_count(app: &mut App) -> usize {
        app.world_mut().query_filtered::<Entity, With<RoomRoot>>().iter(app.world()).count()
    }

    #[test]
    fn exit_room_always_tears_down_and_clears_any_lingering_zoom() {
        // The old leak class this replaces: `Screen::PatchBay` had its own
        // `OnExit` (`exit_patch_bay`, deleted) because a transition leaving
        // the shell FROM the dive bypassed `OnExit(Screen::Room)` entirely.
        // With `zoomed` a resource field, `Screen::Room` is the only state
        // this scene graph ever occupies — every way out runs THIS exit, and
        // there is no more per-target branch for it to skip.
        let mut app = zoom_lifecycle_app();
        app.world_mut().spawn(RoomRoot);
        let cam = app.world_mut().spawn((Camera::default(), RoomCamera)).id();
        app.world_mut().resource_mut::<RoomState>().zoomed = Some(Station::PatchBay);

        set_screen(&mut app, Screen::Room);
        set_screen(&mut app, Screen::Conversation);

        assert_eq!(room_root_count(&mut app), 0, "leaving the room always tears it down");
        assert!(app.world().get::<RoomCamera>(cam).is_none(), "the camera claim is released");
        assert!(
            app.world().resource::<RoomState>().zoomed.is_none(),
            "exit_room clears any lingering zoom so a later re-entry starts unzoomed"
        );
        let theme_bg = app.world().resource::<crate::ui::theme::Theme>().bg;
        let clear = app.world().get::<Camera>(cam).unwrap().clear_color;
        assert!(
            matches!(clear, ClearColorConfig::Custom(c) if c == theme_bg),
            "the conversation clear colour must be restored: {clear:?}"
        );
    }

    #[test]
    fn reserved_marker_height_is_a_low_stub_a_third_of_a_station_pylon() {
        assert!(
            (MARKER_HEIGHT_RESERVED - MARKER_HEIGHT / 3.0).abs() < 1e-4,
            "reserved marker is roughly a third the height of a built station's pylon"
        );
        assert!(MARKER_HEIGHT_RESERVED < MARKER_HEIGHT, "still shorter than a station pylon");
    }

    // ── circuit-board routes (real production config) ──

    #[test]
    fn w_bundle_terminal_pads_cluster_at_the_wall_base_under_the_wheel() {
        // The wall-mount retune (2026-07-10): the W wiring now flows all the
        // way to the wall the wheel hangs on, not to a floor dais foot — pads
        // land past the old marker radius, just short of the panel itself.
        let w_bundle = &route_bundles(&ScenePalette::default())[0];
        assert!(
            w_bundle.pad_range.0 > ROOM_RADIUS,
            "W pads now reach past the old marker radius, toward the wall: {:?}",
            w_bundle.pad_range
        );
        assert!(
            w_bundle.pad_range.1 < palette::WALL_APOTHEM,
            "W pads stop just short of the wall panel itself: {:?}",
            w_bundle.pad_range
        );
        assert!(
            w_bundle.pad_range.0 < w_bundle.pad_range.1,
            "a valid, non-empty pad radius range: {:?}",
            w_bundle.pad_range
        );
    }

    #[test]
    fn route_bundle_total_count_is_within_the_target_board_density() {
        let total: usize = route_bundles(&ScenePalette::default()).iter().map(|b| b.count).sum();
        assert!(
            (24..=36).contains(&total),
            "board density should read dense like concept 06: {total} routes"
        );
    }

    #[test]
    fn every_route_bundle_departs_from_outside_the_console_keepout() {
        assert!(
            RING_OUTER_R - RING_BAND_WIDTH * 0.5 > KEEPOUT_RADIUS,
            "the ring's inner edge clears the keep-out"
        );
    }

    #[test]
    fn every_generated_board_route_clears_the_console_keepout() {
        // The concrete lock the old `TRACE_ARCS` debug_assert covered, applied
        // to the real bundle table + the real ring/chamfer/pad constants
        // `spawn_floor` calls `expand_bundle` with.
        for (bi, bundle) in route_bundles(&ScenePalette::default()).iter().enumerate() {
            let seed_base = bi as u32 * 10_007;
            for route in bearing::expand_bundle(
                bundle,
                RING_OUTER_R,
                ROUTE_CHAMFER,
                ROUTE_ARC_SEGMENTS,
                TRACE_Y,
                ROUTE_PAD_RADIUS_RANGE,
                seed_base,
            ) {
                for p in &route.points {
                    let r = (p[0] * p[0] + p[2] * p[2]).sqrt();
                    assert!(
                        r > KEEPOUT_RADIUS,
                        "bundle {bi} route point at r={r} crosses the console keep-out"
                    );
                }
            }
        }
    }

    #[test]
    fn every_generated_board_route_crest_is_capped_and_trough_stays_ldr() {
        // The new invariant (2026-07-10, the faint-moving-glow slice) in
        // place of the old flat "< 1.0" check: every route's crest
        // renormalizes to AT MOST `ScenePalette::crest` regardless of its
        // raw identity hue, and both the trace's and the pad's RESTING
        // trough level stay LDR even measured against the crest cap itself
        // (`trough * crest < 1.0`) — the element only ever reads loud
        // at the crest's instantaneous peak, never sustained.
        let palette = ScenePalette::default();
        for (bi, bundle) in route_bundles(&palette).iter().enumerate() {
            let seed_base = bi as u32 * 10_007;
            for route in bearing::expand_bundle(
                bundle,
                RING_OUTER_R,
                ROUTE_CHAMFER,
                ROUTE_ARC_SEGMENTS,
                TRACE_Y,
                ROUTE_PAD_RADIUS_RANGE,
                seed_base,
            ) {
                let identity = [
                    route.hue[0] * route.brightness_scale,
                    route.hue[1] * route.brightness_scale,
                    route.hue[2] * route.brightness_scale,
                ];
                let crest = crest_color(identity, palette.crest);
                let max_channel = crest.x.max(crest.y).max(crest.z);
                assert!(
                    max_channel <= palette.crest + 1e-4,
                    "bundle {bi} crest exceeds the cap: {crest:?}"
                );
            }
        }
        assert!(
            palette.trough_wiring * palette.crest < 1.0,
            "trace trough must stay LDR even against the crest cap"
        );
        assert!(
            palette.trough_pads * palette.crest < 1.0,
            "pad trough must stay LDR even against the crest cap"
        );
    }

    #[test]
    fn floor_mesh_carries_a_vertex_colour_gradient() {
        let mesh = floor_mesh(FLOOR_RADIUS, 6, 24);
        assert!(
            mesh.attribute(Mesh::ATTRIBUTE_COLOR).is_some(),
            "the floor gradient rides on vertex colours"
        );
    }

    // ── pylon plinths + gold caps ──

    #[test]
    fn every_built_station_wants_a_gold_cap() {
        for wp in bearing::wall_placements() {
            assert_eq!(
                wants_gold_cap(&wp),
                wp.station.is_some(),
                "{:?}: cap gating must track whether a station stands there",
                wp.bearing
            );
        }
    }

    #[test]
    fn the_reserved_south_marker_stays_plinth_only() {
        let south = bearing::wall_placements()
            .into_iter()
            .find(|wp| wp.bearing == Bearing::South)
            .expect("south has a wall placement");
        assert!(!wants_gold_cap(&south), "the reserved bearing stays humble — no gold cap");
    }

    // ── the well table ──

    #[test]
    fn the_table_plinth_sits_inside_the_console_keepout() {
        // "MORE SOLIDNESS": the table's foot should read as *why* the floor
        // traces can't cross the console, not just coincide with it.
        assert!(TABLE_PLINTH_RADIUS <= KEEPOUT_RADIUS, "the plinth fills the keep-out, not past it");
    }

    #[test]
    fn the_table_pedestal_is_narrower_than_the_tabletop_and_the_plinth_is_wider() {
        assert!(TABLE_PEDESTAL_RADIUS < TABLE_RADIUS, "pedestal narrower than the tabletop");
        assert!(TABLE_PLINTH_RADIUS > TABLE_RADIUS, "plinth wider than the tabletop");
    }

    // `console_rings_float_above_the_tabletop` (the old `CONSOLE_RINGS`
    // placeholder's own test) is retired along with the const it tested
    // (Slice C) — the equivalent claim for the real well now lives in
    // `time_well::scene`'s `station_center_placement_seats_above_the_room_
    // table_with_no_rotation`.

    // ── octagon wall shell (room-level constants) ──

    #[test]
    fn wall_apothem_clears_the_old_radiator_radius_and_the_marker_radius() {
        // WALL_APOTHEM moved to `palette.rs` (2026-07-10, the wall-mount
        // slice) — a cross-file datum now that `patch_bay`'s placement reads
        // it too, but `room::spawn_walls` still builds the panel geometry at
        // this same radius.
        assert!(
            palette::WALL_APOTHEM > ROOM_RADIUS,
            "the shell must enclose the wall stations: {}",
            palette::WALL_APOTHEM
        );
        assert!(palette::WALL_APOTHEM > 660.0, "the shell must enclose the old radiator radius (660)");
        assert!(palette::WALL_APOTHEM < FLOOR_RADIUS, "the shell must stand on the floor disc");
    }

    #[test]
    fn floor_radius_clears_the_octagon_circumradius_so_the_walls_stand_on_the_floor() {
        // The binding constraint the 2026-07-10 evening apothem bump (800 →
        // 1200) introduced: the octagon's own VERTEX (where two panels meet)
        // sits a touch past the apothem itself (`octagon_circumradius`), so
        // the weaker `WALL_APOTHEM < FLOOR_RADIUS` check above isn't enough —
        // the floor has to reach past every corner, or the walls stand past
        // the disc's edge instead of on it.
        let circumradius = bearing::octagon_circumradius(palette::WALL_APOTHEM);
        assert!(
            FLOOR_RADIUS > circumradius,
            "the floor disc must reach past every octagon vertex ({circumradius}) so the walls stand ON it: {FLOOR_RADIUS}"
        );
    }

    #[test]
    fn wall_panel_width_stays_positive_after_the_mullion_gap() {
        let width = bearing::octagon_panel_width(palette::WALL_APOTHEM) - WALL_PANEL_GAP;
        assert!(width > 0.0, "the mullion gap must not eat the whole panel: {width}");
    }

    #[test]
    fn wall_trim_glow_trough_stays_in_the_restrained_neon_range_and_ldr_at_the_crest() {
        // Mission spec: "LDR ~0.5-0.7 of hue" for the trim's resting level —
        // still true now that the trim breathes/waves between this trough
        // and a crest capped at `ScenePalette::crest`; also lock the new
        // invariant that even AT the crest cap, the trough reads LDR.
        let palette = ScenePalette::default();
        assert!(
            (0.5..=0.7).contains(&palette.trough_wall_trim),
            "trim trough out of the restrained range: {}",
            palette.trough_wall_trim
        );
        assert!(
            palette.trough_wall_trim * palette.crest < 1.0,
            "trim trough must stay LDR even against the crest cap"
        );
        assert!(WALL_TRIM_GLOW_PERIOD > 0.0, "trim wave period must be positive");
    }

    #[test]
    fn wall_base_and_mullion_colours_stay_ldr() {
        let palette = ScenePalette::default();
        let lum = |c: LinearRgba| c.red + c.green + c.blue;
        assert!(lum(palette.wall_base) < 1.0, "panel base must stay LDR (decoration, not live activity)");
        assert!(lum(palette.wall_mullion) < 1.0, "mullion must stay LDR");
    }

    #[test]
    fn wall_base_is_a_hair_lighter_than_the_dome_rim() {
        let palette = ScenePalette::default();
        let dome_rim = bearing::dome_color(0.0);
        let lum = |c: LinearRgba| c.red + c.green + c.blue;
        assert!(
            lum(palette.wall_base) > dome_rim[0] + dome_rim[1] + dome_rim[2],
            "the panel base should read a hair lighter than the dome's own rim"
        );
    }

    #[test]
    fn wall_thread_jitter_ranges_stay_within_the_panel_and_ldr() {
        assert!(WALL_THREAD_HEIGHT_RANGE.0 > 0.0 && WALL_THREAD_HEIGHT_RANGE.1 <= 1.0);
        assert!(WALL_THREAD_HEIGHT_RANGE.0 < WALL_THREAD_HEIGHT_RANGE.1);
        assert!(WALL_THREAD_BRIGHTNESS_RANGE.1 < 1.0, "thread brightness must stay LDR");
    }

    #[test]
    fn ribbon_mesh_has_matching_position_normal_and_uv_counts() {
        let line = [[0.0, 0.0, 0.0], [50.0, 0.0, 0.0], [100.0, 0.0, 0.0]];
        let mesh = ribbon_mesh(&line, 8.0);
        use bevy::mesh::VertexAttributeValues;
        let pos = mesh.attribute(Mesh::ATTRIBUTE_POSITION).unwrap().len();
        let nrm = mesh.attribute(Mesh::ATTRIBUTE_NORMAL).unwrap().len();
        let uv = mesh.attribute(Mesh::ATTRIBUTE_UV_0).unwrap().len();
        assert_eq!(pos, 6, "2 verts × 3 points");
        assert_eq!(nrm, pos, "one up-normal per vertex");
        assert_eq!(uv, pos, "one uv per vertex — TraceGlowMaterial's mode-0 wave rides it");
        if let Some(VertexAttributeValues::Float32x3(ns)) = mesh.attribute(Mesh::ATTRIBUTE_NORMAL) {
            assert!(ns.iter().all(|n| *n == [0.0, 1.0, 0.0]), "all up");
        }
    }

    #[test]
    fn glow_quad_mesh_puts_uv_x_on_the_longer_dimension() {
        use bevy::mesh::VertexAttributeValues;

        // Wide-and-short (a panel's top/bottom trim): uv.x should track the
        // wide axis (X, matching `Rectangle::mesh()`'s own convention).
        let wide = glow_quad_mesh(100.0, 10.0);
        if let Some(VertexAttributeValues::Float32x2(uvs)) = wide.attribute(Mesh::ATTRIBUTE_UV_0) {
            // Corner 0 sits at (+hw, +hh); its u should be 1 (max X).
            assert_eq!(uvs[0][0], 1.0, "wide quad: uv.x tracks X: {uvs:?}");
        } else {
            panic!("expected Float32x2 uvs");
        }

        // Narrow-and-tall (a panel's left/right trim): uv.x should track Y
        // instead — the long axis here — not X (the thin thickness).
        let tall = glow_quad_mesh(10.0, 100.0);
        if let Some(VertexAttributeValues::Float32x2(uvs)) = tall.attribute(Mesh::ATTRIBUTE_UV_0) {
            // Corner 0 sits at (+hw, +hh); its u should still be 1 (max Y now).
            assert_eq!(uvs[0][0], 1.0, "tall quad: uv.x tracks Y: {uvs:?}");
            // Corner 3 sits at (+hw, -hh): same X as corner 0, different Y —
            // its u must differ from corner 0's if u truly tracks Y not X.
            assert_ne!(uvs[0][0], uvs[3][0], "u must vary along the long (Y) axis: {uvs:?}");
        } else {
            panic!("expected Float32x2 uvs");
        }
    }

    #[test]
    fn dome_mesh_carries_a_vertex_colour_gradient() {
        let mesh = dome_mesh(100.0);
        assert!(
            mesh.attribute(Mesh::ATTRIBUTE_COLOR).is_some(),
            "the vault gradient rides on vertex colours"
        );
    }

    #[test]
    fn a_beat_sync_warms_the_east_tracks_bearing_through_the_ingest_system() {
        // The acceptance path end-to-end at the resource level: a jam's BeatSync
        // event, ingested ungated, lifts the East (tracks) bearing's activity —
        // what `sync_room_glow` then turns into the marker's breath.
        let mut app = App::new();
        app.add_plugins(bevy::time::TimePlugin)
            .init_resource::<BearingActivity>()
            .add_message::<ServerEventMessage>()
            .add_systems(Update, ingest_room_activity);

        app.world_mut()
            .write_message(ServerEventMessage(kaijutsu_client::ServerEvent::BeatSync {
                context_id: kaijutsu_types::ContextId::from_bytes([7; 16]),
                beat_ref: kaijutsu_audio::BeatRef::new(0.0, 2.0),
            }));
        app.update();

        let act = app.world().resource::<BearingActivity>();
        assert!(act.level(Bearing::East) > 0.0, "the tracks bearing warmed");
        assert_eq!(act.level(Bearing::Center), 0.0, "console stayed dark");
    }
}
