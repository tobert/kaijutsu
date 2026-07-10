//! Room level — the shell's **Tardis chamber** (`docs/scenes/shell.md`, slice A:
//! "the room exists"). A circular vaulted room that holds the stations at
//! stable compass **bearings** around a central **console** (an emblem of the
//! time well). This is the pull-back level above the well: Up-Up at the well's
//! mouth ring enters it (the speedbumped edge, [`WellEdgeBump`]); Left/Right
//! cycle the station carousel, Enter/Down dive into a built station, Esc drops
//! to the conversation. Diving into a station **cuts** to its dedicated scene;
//! the shell never renders a station's detail.
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
//!   HDR emission only on live activity; all decoration stays LDR.
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
//! bearing spawns no marker/plinth/cap/nameplate at all — [`spawn_w_dais`]
//! builds a dais there instead, and the patch bay's own placement (untouched
//! here) seats the wheel on it: the wheel **is** the station.
//!
//! Materials are all built-in [`StandardMaterial`] with `unlit: true`, carrying
//! brightness in `base_color` — LDR (< 1.0 linear) reads crisp, HDR (> 1.0)
//! blooms through the app camera's threshold-1.0 bloom (`main::setup_camera`).
//! No new WGSL, no image assets, no new fonts (the charter's procedural-first
//! budget rule).

pub mod activity;
pub mod bearing;
pub mod nav;

use std::time::Instant;

use bevy::prelude::*;

use activity::BearingActivity;
use bearing::Bearing;
use nav::{DoubleTap, Station, StationCarousel};

use crate::connection::actor_plugin::ServerEventMessage;
use crate::shaders::WellCardMaterial;
use crate::text::ShapingFonts;
use crate::text::components::bevy_color_to_brush;
use crate::text::msdf::{
    FontDataMap, MsdfAtlas, MsdfBlockGlyphs, PositionedGlyph, collect_msdf_glyphs,
};
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};
use crate::ui::screen::{Screen, in_shell};
use crate::view::palette;
use crate::view::patch_bay;
use crate::view::time_well::live::WellBeats;
use crate::view::time_well::panel::{commit_panel_glyphs, create_msdf_panel};
use vello::peniko::Brush;

// ── Room palette + geometry (Amy-tunable) ───────────────────────────────────

/// Room background clear — a shade darker than the well's, so leaving the well
/// upward reads as stepping back into the larger dark of the room.
const ROOM_BG: Color = Color::srgb(0.020, 0.026, 0.044);

/// Floor disc radius (world units); comfortably past the wall stations so the
/// room reads as a chamber, not a platform.
const FLOOR_RADIUS: f32 = 1100.0;
/// Floor mesh resolution ([`bearing::disc_vertices`]): concentric rings ×
/// angular segments. Coarse enough to stay cheap, fine enough that the radial
/// gradient ([`bearing::floor_color`]) reads smooth, not banded.
const FLOOR_RINGS: usize = 14;
const FLOOR_SEGMENTS: usize = 64;

/// Enclosing vault-dome radius. Must exceed **every** camera distance (the
/// pulled-back overview sits ~1630 out) so the camera stays *inside* the dome
/// and its far inner surface reads as the vault; the lower hemisphere hides
/// under the floor disc.
const DOME_RADIUS: f32 = 2000.0;

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
const PYLON_PLINTH_COLOR: [f32; 3] = TABLE_COLOR;
/// Cap brightness — the same gold family/weight as the table rim and console
/// rings, so every gold accent in the room reads as one hue.
const PYLON_CAP_GOLD_LDR: f32 = 0.50;

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
/// Where the approach pose *looks* (world-Y at the wall): furniture height,
/// not plate height — the station's instrument is the subject, the plate
/// hangs above it in frame (the W-approach used to aim at the plate and crop
/// the table's bottom out of the shot).
const APPROACH_LOOK_HEIGHT: f32 = 130.0;

// ── Console emblem (gold — the well's reserved hue) ──────────────────────────

/// The console: a stack of gold rings at center — the slice-A stand-in for the
/// live well. `(y, major_radius, minor_radius)` per ring, apex smallest.
/// Thin and widely spaced on purpose: over the tabletop the original chunky
/// close-set tori (minor 5–7, y 14/36/56) overlapped into a solid "gold cake"
/// from the overview; airy rings read as hologram, not pastry. **Amy-tunable.**
const CONSOLE_RINGS: [(f32, f32, f32); 3] =
    [(30.0, 100.0, 3.5), (64.0, 76.0, 3.0), (98.0, 52.0, 2.5)];
/// Console gold hue (linear rgb identity).
const CONSOLE_GOLD_HUE: [f32; 3] = [1.00, 0.78, 0.34];
/// Console rest brightness (LDR — a soft steady glow, no bloom).
const CONSOLE_LDR: f32 = 0.60;
/// Chatter gain: `activity(Center)` (0..1) → this much HDR lift on the console.
const CONSOLE_CHATTER_GAIN: f32 = 2.2;

// ── Well table (Amy: "MORE SOLIDNESS" — heavy furniture, not hologram) ──────

/// The table the console rings hover above: a chunky tabletop, a gold rim,
/// a narrower pedestal, and a wide low plinth grounding it all to the floor.
/// `enter_room` shifts every [`CONSOLE_RINGS`] entry up by `TABLE_TOP_Y` so
/// the ring stack floats above the top face — the rings' material,
/// [`ConsoleEmblem`], and the chatter glow are untouched.
const TABLE_TOP_Y: f32 = 70.0;
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
/// Dark tabletop/pedestal/plinth colour — a shade lighter than the floor.
const TABLE_COLOR: [f32; 3] = [0.032, 0.036, 0.050];
/// Gold trim brightness (rim torus): the same LDR weight as the console rings.
const TABLE_GOLD_LDR: f32 = 0.50;

// ── Station W dais (the wheel IS the west station; `docs/scenes/palette.rs`'s
// station-W contract) ────────────────────────────────────────────────────────
// No marker pylon stands at W any more — the patch bay's own wheel is the
// station ([`spawn_w_dais`]). Position/scale of the wheel itself lives in
// `patch_bay.rs`'s `STATION_W_PLACEMENT` (untouched here); the two files agree
// only through the shared `palette::STATION_W_*` constants, so neither can
// drift without the other noticing.

/// Foot plinth radius — a touch wider than the dais top, grounding it (the
/// well table's plinth-wider-than-tabletop read, [`spawn_table`]).
const W_DAIS_FOOT_R: f32 = palette::STATION_W_DAIS_R + 22.0;
/// Foot plinth height — the table plinth's own weight.
const W_DAIS_FOOT_HEIGHT: f32 = TABLE_PLINTH_HEIGHT;
/// Gold rim torus at the dais top edge — the table rim's weight and hue.
const W_DAIS_RIM_MINOR: f32 = TABLE_RIM_MINOR;

// ── Octagon wall shell (the enclosing chamber; `shell.md`'s cutaway
// centerpiece) ──────────────────────────────────────────────────────────────
// Eight flat single-sided panels forming a wall around the room — faces on
// the four cardinals plus the four `bearing::RADIATOR_DIRS` diagonals,
// `WALL_APOTHEM` out from center ([`bearing::octagon_panels`]). Every
// panel/mullion/trim/thread quad MUST stay single-sided (default
// `cull_mode`, no `Cuboid`, no `cull_mode: None`): a camera outside the
// octagon sees a near panel's back face — culled — and the chamber shows
// through (the dollhouse cutaway; see `bearing`'s own module comment for the
// exact mechanics).

/// Apothem (center-to-face distance): clears the old radiator radius (660)
/// and the wall-station radius the pylons/markers stand at (`ROOM_RADIUS`,
/// 620), so the shell encloses everything already standing in the room.
const WALL_APOTHEM: f32 = 800.0;
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
/// Dark glass base — a hair lighter than the dome's rim
/// ([`bearing::dome_color`]`(0.0)`, `[0.050, 0.048, 0.086]`) so a panel reads
/// as a surface catching the vault's glow, not a hole in it.
const WALL_BASE_COLOR: [f32; 3] = [0.062, 0.060, 0.094];
/// Mullion colour — a shade darker than the panel base: unlit structure
/// between faces of different hues, no identity hue of its own.
const WALL_MULLION_COLOR: [f32; 3] = [0.040, 0.040, 0.058];
/// Edge-trim strip thickness, and how far it floats proud of the panel base
/// (inward, along the panel's own local +Z — the "proud" idiom the old
/// radiator thread-strips used, now needed for the trim too since both are
/// now zero-thickness quads that would otherwise share a plane).
const WALL_TRIM_THICKNESS: f32 = 12.0;
const WALL_TRIM_PROUD: f32 = 2.2;
/// Trim brightness — restrained neon, not a blown highlight (mission's
/// "LDR ~0.5-0.7 of hue"; stays LDR).
const WALL_TRIM_LDR: f32 = 0.60;

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
/// Etched fabric hues (linear rgb, dim): crimson = MIDI, cyan = PCM, green =
/// VFS (dimmed from the [0.40, 0.85, 0.52] marker identity for etching
/// headroom), gold = the well (used sparingly — gold is the console's hue).
/// At rest a trace is a dark engraving; it lights HDR only when its flow runs
/// (later slices). One hue family per fabric (the charter's rainbow-board
/// rule); the violet stubs reuse [`palette::VIOLET_GLASS`] directly.
const TRACE_CRIMSON: [f32; 3] = [0.24, 0.055, 0.070];
const TRACE_CYAN: [f32; 3] = [0.050, 0.170, 0.210];
const TRACE_GREEN: [f32; 3] = [0.100, 0.260, 0.150];
const TRACE_GOLD: [f32; 3] = [0.300, 0.220, 0.100];

/// Inscribed gold double-ring routes depart from (concept 06's gold circle
/// band): outer/inner center radius and band width per ring. Both clear
/// [`KEEPOUT_RADIUS`] with room to spare.
const RING_OUTER_R: f32 = 230.0;
const RING_INNER_R: f32 = 190.0;
const RING_BAND_WIDTH: f32 = 10.0;
const RING_GOLD_HUE: [f32; 3] = [1.00, 0.80, 0.36];
const RING_LDR: f32 = 0.50;

/// Route-generation shared geometry ([`bearing::expand_bundle`]): the radial
/// length each 45°-ish bend cuts off the lane radius, and the arc's sample
/// density.
const ROUTE_CHAMFER: f32 = 18.0;
const ROUTE_ARC_SEGMENTS: usize = 14;
/// Terminal-pad disc radius range (world units) — "slightly brighter than
/// their trace."
const ROUTE_PAD_RADIUS_RANGE: (f32, f32) = (6.0, 14.0);
const ROUTE_PAD_BRIGHTNESS_GAIN: f32 = 1.35;

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

/// Approach-pose eye radius: how far out from center the camera stands when
/// facing a wall station — between the console and the wall, on the SAME
/// side as the focus ("walk toward the station you're studying", not sit
/// across the room staring back through the console and the diametrically
/// opposite pylon — the occlusion bug this constant fixes). Roughly a
/// quarter of the wall radius the markers actually stand at (`ROOM_RADIUS`).
const ROOM_CAM_APPROACH_R: f32 = 160.0;
/// Approach-pose eye height — the old orbit camera's focused-pose lift,
/// carried over unchanged (a comfortable "person standing" height).
const ROOM_CAM_APPROACH_HEIGHT: f32 = 260.0;
/// The console (TimeWell) overview pose — pulled back from the south, framing
/// the *whole* room so every bearing's ambient glow reads at once (the tracks
/// (E) marker must breathe here without diving — the slice-A acceptance).
/// Lowered 2026-07-10 from the original (0, 640, 1500) high shot toward the
/// concepts' human-eye framing (06 stands *in* the room, not above it): the
/// table + ring stack sit mid-frame with the stations standing around them.
/// **Amy-tunable** (the lead live-tunes the exact framing).
const OVERVIEW_POS: Vec3 = Vec3::new(0.0, 340.0, 1380.0);
const OVERVIEW_LOOK: Vec3 = Vec3::new(0.0, 90.0, 0.0);
/// Camera follow rate (exponential smoothing) — matches the well's weighty
/// glide so the shell and the well feel like one instrument.
const CAMERA_EASE_RATE: f32 = 4.0;

// ── Ambient glow gains ───────────────────────────────────────────────────────

/// Marker rest brightness (LDR multiplier on the marker's identity hue).
const MARKER_LDR: f32 = 0.42;
/// Tracks (E) beat gain: `global_envelope` (0..1) → this much HDR lift on the
/// tracks marker each beat — the acceptance "breathe" (`shell.md` slice A).
const TRACKS_BEAT_GAIN: f32 = 2.8;
/// Sustained lift under the beat while a track is rolling (`activity(East)`).
const TRACKS_ACTIVE_GAIN: f32 = 0.5;
/// Steady brightness lift on the focused station's marker/console.
const FOCUS_LIFT: f32 = 0.35;
/// Quantization step for the glow lanes — coarse enough that a settled marker
/// stops re-extracting its material (the well's `LIVE_LANE_STEP` discipline).
const GLOW_STEP: f32 = 1.0 / 64.0;

/// The well-edge speedbump window (ms) — same 500ms as the app's other
/// double-tap gestures (`input/interrupt.rs`, `input/vim/dismiss.rs`).
const EDGE_BUMP_WINDOW_MS: u128 = 500;

// ── State ─────────────────────────────────────────────────────────────────────

/// Which station the room carousel focuses. Whoever *enters* the room sets the
/// focus first (the well focuses TIME WELL; the patch bay focuses PATCH BAY),
/// so arriving always faces where you came from.
#[derive(Resource)]
pub struct RoomState {
    pub carousel: StationCarousel,
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
        Self { carousel: StationCarousel::new(Station::TimeWell), plates_dirty: true }
    }
}

/// The Up-Up speedbump at the well's mouth ring (`docs/scenes/shell.md`,
/// "Levels — the arrows continue"). Fed by `well_keyboard`; firing exits the
/// well to the room.
#[derive(Resource)]
pub struct WellEdgeBump(pub DoubleTap);

impl Default for WellEdgeBump {
    fn default() -> Self {
        Self(DoubleTap::new(EDGE_BUMP_WINDOW_MS))
    }
}

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

/// A ring of the central console emblem. All rings share one material, glow-lit
/// together with context chatter.
#[derive(Component)]
pub struct ConsoleEmblem;

/// Room chrome that **fades when you dive** into a station (`docs/scenes/shell.md`,
/// slice B): the bearing pylons, station nameplates, console emblem, and violet
/// radiators. `apply_room_dive_visibility` hides these while `Screen::PatchBay`
/// so the dived station owns the eye; the floor, its traces, the vault dome, and
/// the dived station itself stay — they are the chamber, not distractions.
#[derive(Component)]
pub struct RoomDistraction;

// ── Plugin ────────────────────────────────────────────────────────────────────

pub struct RoomPlugin;

impl Plugin for RoomPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RoomState>()
            .init_resource::<WellEdgeBump>()
            .init_resource::<BearingActivity>()
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
            // Shell-wide (room OR patch-bay dive): the camera dolly retargets on
            // the state flip so diving/surfacing reads as one continuous move, and
            // the dive dims the room chrome. Both run across BOTH shell screens
            // (`docs/scenes/shell.md`, slice B — one shared scene graph).
            .add_systems(
                Update,
                (ease_shell_camera, apply_room_dive_visibility).run_if(in_shell),
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
    mut edge_bump: ResMut<WellEdgeBump>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut mats: ResMut<Assets<StandardMaterial>>,
    mut card_mats: ResMut<Assets<WellCardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut app_camera: Query<(Entity, &mut Camera, &mut Transform), With<Camera3d>>,
    existing: Query<Entity, With<RoomRoot>>,
) {
    // Surfacing from a patch-bay dive (`exit_room` kept the room, its W furniture,
    // and the shared camera alive when the target was PatchBay). Nothing to build
    // or claim — `ease_shell_camera` glides the camera back to the W focus, the
    // LOD systems restore the room chrome. Only a *fresh* room arrival (from the
    // well or conversation) falls through to spawn.
    if !existing.is_empty() {
        return;
    }

    arm_on_enter(&mut room);
    // Belt-and-braces: a fresh room entry must never inherit an armed
    // well-edge speedbump. `exit_time_well` resets it on the way out of the
    // well, but that only covers exits *through* the well's own teardown —
    // this is the room's own guarantee, independent of how we got here.
    edge_bump.0.reset();

    // Claim the shared app camera (the well-marker convention: insert to claim,
    // remove + restore clear color to release) and place it facing the entering
    // focus so there's no first-frame snap before the ease takes over.
    if let Ok((cam_entity, mut cam, mut tf)) = app_camera.single_mut() {
        commands.entity(cam_entity).insert(RoomCamera);
        cam.clear_color = ClearColorConfig::Custom(ROOM_BG);
        let (pos, look) = desired_camera(room.carousel.focused_station());
        *tf = Transform::from_translation(pos).looking_at(look, Vec3::Y);
    }

    let root = commands
        .spawn((RoomRoot, Transform::default(), Visibility::Inherited, Name::new("RoomRoot")))
        .id();

    // Floor disc, inscribed gold ring, and circuit-board routes — one cohesive
    // helper (`shell.md`, "the floor is the wiring": the disc, its rings, and
    // its traces are the chamber, not room chrome).
    spawn_floor(&mut commands, root, &mut meshes, &mut mats);

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
    spawn_table(&mut commands, root, &mut meshes, &mut mats);

    // Console emblem — a stack of gold rings hovering above the tabletop (the
    // well's stand-in). One shared material so the chatter glow lifts them
    // together; only the Y offset changed from the bare-floating-stack
    // original — [`ConsoleEmblem`] and the glow system are untouched.
    let console_mat = mats.add(unlit(lin_scaled(CONSOLE_GOLD_HUE, CONSOLE_LDR)));
    for (i, (y, major, minor)) in CONSOLE_RINGS.iter().enumerate() {
        commands.spawn((
            Mesh3d(meshes.add(Torus { minor_radius: *minor, major_radius: *major })),
            MeshMaterial3d(console_mat.clone()),
            Transform::from_xyz(0.0, *y + TABLE_TOP_Y, 0.0),
            Visibility::Inherited,
            ConsoleEmblem,
            // RoomDistraction = the rings dim on a *station* dive today. If a
            // future slice makes the console itself divable by camera descent
            // (the way PatchBay is), this tag would hide the very thing being
            // dived into — re-judge it then (kaibo, 2026-07-10).
            RoomDistraction,
            Name::new(format!("ConsoleRing{i}")),
            ChildOf(root),
        ));
    }

    // Wall stations: a marker pylon at each bearing, plus an engraved nameplate
    // at the labeled ones (the reserved South bearing gets a dim marker only).
    // A furnished bearing (`bearing::station_is_room_furniture` — today just
    // PatchBay/W, "the wheel IS the west station") gets neither: no marker, no
    // plate — `spawn_w_dais` below builds its own furniture instead.
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
        let marker_mat = mats.add(unlit(lin_v(hue * MARKER_LDR)));
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
    spawn_pylons(&mut commands, root, &mut meshes, &mut mats);

    // The W dais — the wheel IS the west station (`shell.md` slice B, retuned
    // 2026-07-10). Not RoomDistraction: it's the station's own furniture, so
    // it stays lit through the dive same as the wheel itself.
    spawn_w_dais(&mut commands, root, &mut meshes, &mut mats);

    // The octagon wall shell: eight single-sided panels enclosing the room,
    // corner mullions, hue-coded edge trim, and — on the four diagonals — the
    // violet information threads that used to stand as free-floating
    // radiators. The chamber, not room chrome (no RoomDistraction).
    spawn_walls(&mut commands, root, &mut meshes, &mut mats);

    // Re-root the patch bay into the room as furniture at the W bearing (slice B,
    // one shared scene graph). It rides `RoomRoot`, so it lives exactly as long as
    // the room; `arm_scene` primes the first observed-graph poll so its chords —
    // the W ambient — build straight away without a dive.
    patch_bay::spawn_furniture(
        &mut commands,
        root,
        &mut meshes,
        &mut mats,
        &mut card_mats,
        &mut images,
    );
    patch_bay::arm_scene(&mut pb_state);

    info!("room: entered (Tardis chamber, slice B — patch bay stationed at W)");
}

pub(crate) fn exit_room(
    mut commands: Commands,
    screen: Res<State<Screen>>,
    theme: Res<crate::ui::theme::Theme>,
    roots: Query<Entity, With<RoomRoot>>,
    mut app_camera: Query<(Entity, &mut Camera), With<RoomCamera>>,
) {
    // Diving into the patch bay is travel WITHIN the shared shell scene graph,
    // not a scene cut: the room chamber, the W furniture, and the shared camera
    // all survive; `ease_shell_camera` dollies down and this teardown is skipped.
    // `State<Screen>` already holds the *target* here — the transition updates it
    // before OnExit runs (bevy_state `internal_apply_state_transition`, in the
    // DependentTransitions set ahead of the exit schedules). Only leaving the
    // shell entirely (Conversation, the well, the editor…) tears the room down;
    // and because OnExit always precedes the next OnEnter, releasing the camera
    // here lets the destination's own OnEnter (e.g. the well) re-claim it cleanly.
    if *screen.get() == Screen::PatchBay {
        return;
    }
    teardown_room(&mut commands, &theme, &roots, &mut app_camera);
    info!("room: exited");
}

/// Tear the room down: despawn `RoomRoot` (recursively — the chamber and all
/// its furniture, the W patch bay included) and release the shared camera
/// (drop the [`RoomCamera`] claim, restore the conversation clear colour).
///
/// Shared by [`exit_room`] and the patch bay's `exit_patch_bay`: with one
/// shared scene graph, a transition can leave the shell FROM the dived screen
/// (a context switch landing while dived reveals the conversation,
/// `view/sync.rs`; an `open_editor` peer signal jumps to the editor). On that
/// path `OnExit(Screen::Room)` never fires — the state being left is
/// `PatchBay` — so the dive's own exit must run this same teardown, or
/// `RoomRoot`, the camera claim, and the room clear colour all leak into the
/// next screen, and `enter_room`'s surfacing early-return later finds the
/// stale root and never rebuilds (the broken-view cascade).
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
/// The W bundle's `pad_range` was retuned 2026-07-10 (`shell.md`, "the wheel
/// IS the west station"): its terminal pads now cluster just past the dais
/// foot ([`palette::STATION_W_DAIS_R`]) instead of out toward the old wall
/// radius, so the crimson wiring visibly flows INTO the station instead of
/// past it.
fn route_bundles() -> [bearing::RouteBundle; 9] {
    use bearing::{Bearing, RouteBundle, dir_theta};
    let west = dir_theta(Bearing::West.dir());
    let east = dir_theta(Bearing::East.dir());
    let north = dir_theta(Bearing::North.dir());
    let south = dir_theta(Bearing::South.dir());
    let [ne, se, sw, nw] = bearing::RADIATOR_DIRS.map(dir_theta);
    [
        RouteBundle {
            center_theta: west,
            spread: 0.50,
            count: 7,
            lane_range: (280.0, 620.0),
            arc_range: (0.25, 0.9),
            pad_range: (palette::STATION_W_DAIS_R + 120.0, 420.0),
            hue: TRACE_CRIMSON,
            brightness_range: (0.7, 1.15),
        },
        RouteBundle {
            center_theta: east,
            spread: 0.50,
            count: 7,
            lane_range: (300.0, 650.0),
            arc_range: (0.25, 0.9),
            pad_range: (450.0, 900.0),
            hue: TRACE_CRIMSON,
            brightness_range: (0.7, 1.15),
        },
        RouteBundle {
            center_theta: north,
            spread: 0.42,
            count: 5,
            lane_range: (270.0, 560.0),
            arc_range: (0.2, 0.75),
            pad_range: (400.0, 820.0),
            hue: TRACE_GREEN,
            brightness_range: (0.75, 1.15),
        },
        RouteBundle {
            center_theta: north,
            spread: 0.55,
            count: 4,
            lane_range: (340.0, 660.0),
            arc_range: (0.2, 0.8),
            pad_range: (460.0, 850.0),
            hue: TRACE_CYAN,
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
            hue: palette::VIOLET_GLASS,
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
            hue: palette::VIOLET_GLASS,
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
            hue: palette::VIOLET_GLASS,
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
            hue: palette::VIOLET_GLASS,
            brightness_range: (0.8, 1.2),
        },
        RouteBundle {
            center_theta: south,
            spread: 0.9,
            count: 2,
            lane_range: (300.0, 500.0),
            arc_range: (0.15, 0.4),
            pad_range: (380.0, 600.0),
            hue: TRACE_GOLD,
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
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
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

    // Inscribed gold double-ring (concept 06's gold circle band): a thin
    // annulus at RING_OUTER_R and another at RING_INNER_R; routes depart from
    // the outer one.
    let ring_mat = mats.add(unlit(lin_scaled(RING_GOLD_HUE, RING_LDR)));
    for r in [RING_OUTER_R, RING_INNER_R] {
        commands.spawn((
            Mesh3d(meshes.add(Annulus::new(r - RING_BAND_WIDTH * 0.5, r + RING_BAND_WIDTH * 0.5))),
            MeshMaterial3d(ring_mat.clone()),
            Transform::from_xyz(0.0, TRACE_Y, 0.0)
                .with_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
            Visibility::Inherited,
            Name::new("ConsoleRingInlay"),
            ChildOf(root),
        ));
    }

    // Circuit-board routes: each bundle expands to several routes; every
    // route gets a ribbon + a slightly brighter terminal pad.
    for (bi, bundle) in route_bundles().iter().enumerate() {
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
            let color = [
                route.hue[0] * route.brightness_scale,
                route.hue[1] * route.brightness_scale,
                route.hue[2] * route.brightness_scale,
            ];
            debug_assert!(color.iter().all(|c| *c < 1.0), "floor trace must stay LDR");

            let mesh = meshes.add(ribbon_mesh(&route.points, TRACE_WIDTH * route.width_scale));
            let mat = mats.add(StandardMaterial {
                base_color: lin(color),
                unlit: true,
                cull_mode: None,
                ..default()
            });
            commands.spawn((
                Mesh3d(mesh),
                MeshMaterial3d(mat),
                Transform::default(),
                Visibility::Inherited,
                Name::new("FloorTrace"),
                ChildOf(root),
            ));

            let pad_color = [
                color[0] * ROUTE_PAD_BRIGHTNESS_GAIN,
                color[1] * ROUTE_PAD_BRIGHTNESS_GAIN,
                color[2] * ROUTE_PAD_BRIGHTNESS_GAIN,
            ];
            debug_assert!(pad_color.iter().all(|c| *c < 1.0), "floor trace pad must stay LDR");
            commands.spawn((
                Mesh3d(meshes.add(Circle::new(route.pad_radius))),
                MeshMaterial3d(mats.add(unlit(lin(pad_color)))),
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

/// The well table the console rings hover above — heavy furniture, not
/// hologram (Amy: "MORE SOLIDNESS"): a chunky dark tabletop, a gold rim torus
/// at its edge, a narrower pedestal, and a wide low plinth grounding it to the
/// floor. Every part is [`RoomDistraction`] (like the console rings today).
fn spawn_table(
    commands: &mut Commands,
    root: Entity,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
) {
    let table_mat = mats.add(unlit(lin(TABLE_COLOR)));
    let gold_mat = mats.add(unlit(lin_scaled(CONSOLE_GOLD_HUE, TABLE_GOLD_LDR)));

    // Plinth — wide and low, grounding the table to the floor.
    commands.spawn((
        Mesh3d(meshes.add(Cylinder::new(TABLE_PLINTH_RADIUS, TABLE_PLINTH_HEIGHT))),
        MeshMaterial3d(table_mat.clone()),
        Transform::from_xyz(0.0, TABLE_PLINTH_HEIGHT * 0.5, 0.0),
        RoomDistraction,
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
        RoomDistraction,
        Visibility::Inherited,
        Name::new("ConsoleTablePedestal"),
        ChildOf(root),
    ));

    // Tabletop — the chunky slab the console rings float above.
    commands.spawn((
        Mesh3d(meshes.add(Cylinder::new(TABLE_RADIUS, TABLE_THICKNESS))),
        MeshMaterial3d(table_mat),
        Transform::from_xyz(0.0, TABLE_TOP_Y - TABLE_THICKNESS * 0.5, 0.0),
        RoomDistraction,
        Visibility::Inherited,
        Name::new("ConsoleTableTop"),
        ChildOf(root),
    ));

    // Gold rim torus at the tabletop's edge.
    commands.spawn((
        Mesh3d(meshes.add(Torus { minor_radius: TABLE_RIM_MINOR, major_radius: TABLE_RADIUS })),
        MeshMaterial3d(gold_mat),
        Transform::from_xyz(0.0, TABLE_TOP_Y, 0.0),
        RoomDistraction,
        Visibility::Inherited,
        Name::new("ConsoleTableRim"),
        ChildOf(root),
    ));
}

/// The dais at the W bearing — the wheel IS the west station (`shell.md`
/// slice B, retuned 2026-07-10): a low wide cylinder at the palette
/// contract's coordinates, a foot plinth grounding it, and a gold rim ring —
/// the same furniture language as the well table ([`spawn_table`]). NOT
/// [`RoomDistraction`]: it is the station's OWN furniture (the patch wheel
/// stands on it), so it stays lit through a dive same as the wheel itself.
fn spawn_w_dais(
    commands: &mut Commands,
    root: Entity,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
) {
    let dais_mat = mats.add(unlit(lin(palette::DARK_SURFACE)));
    let rim_mat = mats.add(unlit(lin_scaled(palette::GOLD_HUE, palette::GOLD_LDR_TRIM)));

    commands.spawn((
        Mesh3d(meshes.add(Cylinder::new(W_DAIS_FOOT_R, W_DAIS_FOOT_HEIGHT))),
        MeshMaterial3d(dais_mat.clone()),
        Transform::from_xyz(palette::STATION_W_X, W_DAIS_FOOT_HEIGHT * 0.5, 0.0),
        Visibility::Inherited,
        Name::new("StationWDaisFoot"),
        ChildOf(root),
    ));

    let body_height = palette::STATION_W_DAIS_TOP_Y - W_DAIS_FOOT_HEIGHT;
    commands.spawn((
        Mesh3d(meshes.add(Cylinder::new(palette::STATION_W_DAIS_R, body_height))),
        MeshMaterial3d(dais_mat),
        Transform::from_xyz(palette::STATION_W_X, W_DAIS_FOOT_HEIGHT + body_height * 0.5, 0.0),
        Visibility::Inherited,
        Name::new("StationWDais"),
        ChildOf(root),
    ));

    commands.spawn((
        Mesh3d(meshes.add(Torus {
            minor_radius: W_DAIS_RIM_MINOR,
            major_radius: palette::STATION_W_DAIS_R,
        })),
        MeshMaterial3d(rim_mat),
        Transform::from_xyz(palette::STATION_W_X, palette::STATION_W_DAIS_TOP_Y, 0.0),
        Visibility::Inherited,
        Name::new("StationWDaisRim"),
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
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
) {
    let panel_width = bearing::octagon_panel_width(WALL_APOTHEM) - WALL_PANEL_GAP;

    let base_mesh = meshes.add(Rectangle::new(panel_width, WALL_HEIGHT));
    let base_mat = mats.add(unlit(lin(WALL_BASE_COLOR)));
    let h_trim_mesh = meshes.add(Rectangle::new(panel_width, WALL_TRIM_THICKNESS));
    let v_trim_mesh = meshes.add(Rectangle::new(WALL_TRIM_THICKNESS, WALL_HEIGHT));
    let mullion_mesh = meshes.add(Rectangle::new(WALL_MULLION_WIDTH, WALL_HEIGHT));
    let mullion_mat = mats.add(unlit(lin(WALL_MULLION_COLOR)));

    // One trim material per identity hue: the four cardinal bearing hues
    // (read straight off `wall_placements`, so a re-tuned marker hue can't
    // drift out of sync with its wall) plus one shared violet for every
    // diagonal face.
    let placements = bearing::wall_placements();
    let hue_for = |b: Bearing| -> [f32; 3] {
        placements
            .iter()
            .find(|wp| wp.bearing == b)
            .map(|wp| wp.hue)
            .expect("wall_placements always covers all four cardinal bearings")
    };
    let trim_mat_n = mats.add(unlit(lin_scaled(hue_for(Bearing::North), WALL_TRIM_LDR)));
    let trim_mat_e = mats.add(unlit(lin_scaled(hue_for(Bearing::East), WALL_TRIM_LDR)));
    let trim_mat_s = mats.add(unlit(lin_scaled(hue_for(Bearing::South), WALL_TRIM_LDR)));
    let trim_mat_w = mats.add(unlit(lin_scaled(hue_for(Bearing::West), WALL_TRIM_LDR)));
    let trim_mat_violet = mats.add(unlit(lin_scaled(palette::VIOLET_THREAD, WALL_TRIM_LDR)));

    for (i, panel) in bearing::octagon_panels(WALL_APOTHEM).iter().enumerate() {
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

        let trim_mat = match panel.bearing {
            Some(Bearing::North) => trim_mat_n.clone(),
            Some(Bearing::East) => trim_mat_e.clone(),
            Some(Bearing::South) => trim_mat_s.clone(),
            Some(Bearing::West) => trim_mat_w.clone(),
            Some(Bearing::Center) => unreachable!("octagon panels never carry the center bearing"),
            None => trim_mat_violet.clone(),
        };
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
                    MeshMaterial3d(mats.add(unlit(lin_scaled(palette::VIOLET_THREAD, brightness)))),
                    Transform::from_translation(panel_tf.transform_point(local))
                        .with_rotation(panel_tf.rotation),
                    Visibility::Inherited,
                    Name::new(format!("WallPanel{i}Thread{j}")),
                    ChildOf(root),
                ));
            }
        }
    }

    for (i, (pos, _theta)) in bearing::octagon_corners(WALL_APOTHEM).iter().enumerate() {
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
/// [`spawn_w_dais`] builds its own foot/rim instead.
fn spawn_pylons(
    commands: &mut Commands,
    root: Entity,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
) {
    let plinth_mesh =
        meshes.add(Cuboid::new(PYLON_PLINTH_WIDTH, PYLON_PLINTH_HEIGHT, PYLON_PLINTH_WIDTH));
    let plinth_mat = mats.add(unlit(lin(PYLON_PLINTH_COLOR)));
    let cap_mesh = meshes.add(Cuboid::new(PYLON_CAP_WIDTH, PYLON_CAP_HEIGHT, PYLON_CAP_WIDTH));
    let cap_mat = mats.add(unlit(lin_scaled(CONSOLE_GOLD_HUE, PYLON_CAP_GOLD_LDR)));

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

/// Room keys: Left/Right cycle the carousel, Enter/Down dive into a built
/// station, Esc drops to the conversation (the room is the top level). The nav
/// contract is frozen — this is unchanged from the blockout.
fn room_keyboard(
    keys: Res<ButtonInput<KeyCode>>,
    mut room: ResMut<RoomState>,
    mut next: ResMut<NextState<Screen>>,
) {
    if keys.just_pressed(KeyCode::ArrowRight) || keys.just_pressed(KeyCode::Tab) {
        room.carousel.step(1);
    } else if keys.just_pressed(KeyCode::ArrowLeft) {
        room.carousel.step(-1);
    }

    if keys.just_pressed(KeyCode::Enter) || keys.just_pressed(KeyCode::ArrowDown) {
        match room.carousel.focused_station() {
            Station::TimeWell => next.set(Screen::TimeWell),
            Station::PatchBay => next.set(Screen::PatchBay),
            // Unbuilt stations: stay put (the dimmed plate says why).
            _ => {}
        }
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
/// fine: the camera's approach onto the dais is the feedback instead.
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

/// Ease the shared shell camera toward its target pose — travel by intent, the
/// same exponentially-smoothed tween as the well's `ease_camera_to_focused_ring`
/// (no cuts). Runs across BOTH shell screens and reads the target from the state:
/// in the room it faces the focused station's bearing; on a patch-bay dive it
/// descends to [`patch_bay::dive_camera_pose`] (the standalone scene's pose
/// mapped through the W placement). One system, one camera — so diving and
/// surfacing are the SAME continuous glide, just retargeted the frame the state
/// flips (no snap either way).
fn ease_shell_camera(
    time: Res<Time>,
    room: Res<RoomState>,
    screen: Res<State<Screen>>,
    mut cam: Query<&mut Transform, With<RoomCamera>>,
) {
    let Ok(mut tf) = cam.single_mut() else {
        return;
    };
    let (pos, look) = match *screen.get() {
        Screen::PatchBay => patch_bay::dive_camera_pose(),
        _ => desired_camera(room.carousel.focused_station()),
    };
    let desired = Transform::from_translation(pos).looking_at(look, Vec3::Y);
    let alpha = 1.0 - (-CAMERA_EASE_RATE * time.delta_secs()).exp();
    tf.translation = tf.translation.lerp(desired.translation, alpha);
    tf.rotation = tf.rotation.slerp(desired.rotation, alpha);
}

/// Fade the room chrome on a dive: hide the [`RoomDistraction`] chrome (bearing
/// pylons, nameplates, console emblem, radiators) while `Screen::PatchBay` so the
/// dived station owns the eye, and restore it in the room. The floor, its traces,
/// the vault dome, and the dived station itself stay — they are the chamber, not
/// distractions. One mechanism (Visibility), change-guarded so settled chrome
/// never re-dirties (`docs/scenes/shell.md`, slice B — the dived view earns its
/// focus by hiding distractions and showing the labels).
fn apply_room_dive_visibility(
    screen: Res<State<Screen>>,
    mut chrome: Query<&mut Visibility, With<RoomDistraction>>,
) {
    let want = if *screen.get() == Screen::PatchBay {
        Visibility::Hidden
    } else {
        Visibility::Inherited
    };
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

/// Push ambient telemetry into the markers + console as light: the tracks (E)
/// marker breathes with the well's beat phasor (HDR pulse decaying to LDR), the
/// console emblem glows with context chatter, and the focused element takes a
/// steady lift. Change-guarded + quantized so a settled marker never touches
/// `Assets<StandardMaterial>` (the well's `sync_card_live_uniforms` discipline).
fn sync_room_glow(
    room_activity: Res<BearingActivity>,
    beats: Res<WellBeats>,
    room: Res<RoomState>,
    mut mats: ResMut<Assets<StandardMaterial>>,
    markers: Query<(&BearingMarker, &MeshMaterial3d<StandardMaterial>)>,
    console: Query<&MeshMaterial3d<StandardMaterial>, With<ConsoleEmblem>>,
) {
    let now = Instant::now();
    let beat = beats.global_envelope(now);
    let focused = room.carousel.focused_station();

    for (marker, handle) in markers.iter() {
        let mut lift = 0.0;
        if marker.bearing == Bearing::East {
            lift += beat * TRACKS_BEAT_GAIN
                + room_activity.normalized(Bearing::East) * TRACKS_ACTIVE_GAIN;
        }
        if marker.station == Some(focused) {
            lift += FOCUS_LIFT;
        }
        let brightness = quantize(MARKER_LDR + lift);
        set_glow(&mut mats, &handle.0, marker.hue * brightness);
    }

    let mut c_lift = room_activity.normalized(Bearing::Center) * CONSOLE_CHATTER_GAIN;
    if focused == Station::TimeWell {
        c_lift += FOCUS_LIFT;
    }
    let c_target = Vec3::from_array(CONSOLE_GOLD_HUE) * quantize(CONSOLE_LDR + c_lift);
    for handle in console.iter() {
        set_glow(&mut mats, &handle.0, c_target);
    }
}

// ── Camera pose helper ────────────────────────────────────────────────────────

/// The desired `(position, look-at)` for the room camera facing `station` —
/// overview for the console; for a wall station, an **approach** pose: the
/// camera stands on the same side of the room as the focus, between the
/// console and the wall, looking outward at the station's marker/nameplate.
/// Never sits on the opposite wall staring back through the console and the
/// diametrically opposite pylon — both used to sit *in front of* the camera,
/// fully occluding the focused station (the bug this pose replaces). The
/// `Radiators` focus (the NE diagonal, `bearing::RADIATOR_FOCUS_DIR`) rides
/// this same math unchanged.
fn desired_camera(station: Station) -> (Vec3, Vec3) {
    match bearing::focus_dir(station) {
        None => (OVERVIEW_POS, OVERVIEW_LOOK),
        Some(d) => {
            // Look at the marker's own wall radius (ROOM_RADIUS — the same
            // radius the pylons spawn at) held at furniture height
            // (APPROACH_LOOK_HEIGHT): the station's instrument is the
            // subject; its plate hangs above it in the frame. Radiators is
            // the one exception (2026-07-10): its NE panel is now the
            // octagon wall shell's own diagonal face, standing at
            // `WALL_APOTHEM` (800) — well past the old radiator radius (660)
            // this look-point used to target. Left at ROOM_RADIUS (620) the
            // camera would look at empty air short of the wall; every other
            // wall station still has its marker at ROOM_RADIUS, unchanged.
            let wall_r = if station == Station::Radiators { WALL_APOTHEM } else { ROOM_RADIUS };
            (
                Vec3::from_array(bearing::approach_camera(
                    d,
                    ROOM_CAM_APPROACH_R,
                    ROOM_CAM_APPROACH_HEIGHT,
                )),
                Vec3::from_array(bearing::approach_look(d, wall_r, APPROACH_LOOK_HEIGHT)),
            )
        }
    }
}

// ── Material + colour helpers ──────────────────────────────────────────────────

/// An unlit [`StandardMaterial`] carrying its brightness in `base_color` — the
/// room's one emission channel (HDR blooms, LDR reads crisp).
fn unlit(base_color: Color) -> StandardMaterial {
    StandardMaterial { base_color, unlit: true, ..default() }
}

/// A linear-rgb [`Color`] from an `[f32; 3]` (values may exceed 1.0 for HDR).
fn lin(c: [f32; 3]) -> Color {
    Color::LinearRgba(LinearRgba::rgb(c[0], c[1], c[2]))
}

/// [`lin`] scaled by a brightness multiplier.
fn lin_scaled(c: [f32; 3], k: f32) -> Color {
    Color::LinearRgba(LinearRgba::rgb(c[0] * k, c[1] * k, c[2] * k))
}

/// A linear-rgb [`Color`] from a [`Vec3`].
fn lin_v(v: Vec3) -> Color {
    Color::LinearRgba(LinearRgba::rgb(v.x, v.y, v.z))
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
/// this wraps it into a `Mesh` with up-normals.
fn ribbon_mesh(points: &[[f32; 3]], width: f32) -> Mesh {
    use bevy::asset::RenderAssetUsages;
    use bevy::mesh::{Indices, PrimitiveTopology};

    let (positions, indices) = bearing::ribbon_vertices(points, width);
    let normals = vec![[0.0, 1.0, 0.0]; positions.len()];
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_indices(Indices::U32(indices))
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
    /// plates — and nothing has re-armed it for the next entry yet.
    fn persisted_after_a_visit() -> RoomState {
        RoomState { carousel: StationCarousel::new(Station::PatchBay), plates_dirty: false }
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

    #[test]
    fn desired_camera_frames_console_from_the_overview() {
        let (pos, look) = desired_camera(Station::TimeWell);
        assert_eq!(pos, OVERVIEW_POS);
        assert_eq!(look, OVERVIEW_LOOK);
    }

    #[test]
    fn desired_camera_approaches_the_tracks_wall_from_the_same_side() {
        // Tracks is East (+X). The camera now stands on the SAME side as the
        // focus — walking toward the station, not sitting on the opposite
        // wall staring back through the console and the (occluding) west
        // pylon.
        let (pos, look) = desired_camera(Station::Tracks);
        assert!(pos.x > 0.0, "camera stands on the same (east) side: {pos:?}");
        assert!(pos.x < ROOM_RADIUS, "the eye stops well short of the wall: {pos:?}");
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
            let (pos, look) = desired_camera(s);
            let d = bearing::focus_dir(s).expect("wall station has a bearing");
            let eye_r = pos.x * d[0] + pos.z * d[2];
            let look_r = look.x * d[0] + look.z * d[2];
            assert!(eye_r > KEEPOUT_RADIUS, "{s:?} eye clears the console keep-out: {eye_r}");
            assert!(look_r > eye_r, "{s:?} look point sits farther out than the eye: eye={eye_r} look={look_r}");
        }
    }

    #[test]
    fn radiators_focus_looks_at_the_new_wall_apothem_not_the_old_room_radius() {
        // 2026-07-10: the NE radiator panel is now the octagon shell's own
        // diagonal wall face at WALL_APOTHEM (800), not the old free-floating
        // slab at 660 — the look point must have moved out to meet it, and
        // every OTHER wall station's look point must be untouched.
        let (_, look) = desired_camera(Station::Radiators);
        let d = bearing::focus_dir(Station::Radiators).unwrap();
        let look_r = look.x * d[0] + look.z * d[2];
        assert!(
            (look_r - WALL_APOTHEM).abs() < 1e-3,
            "Radiators should look at the wall apothem: {look_r}"
        );
        for s in [Station::PatchBay, Station::Tracks, Station::Vfs] {
            let (_, look) = desired_camera(s);
            let d = bearing::focus_dir(s).unwrap();
            let look_r = look.x * d[0] + look.z * d[2];
            assert!(
                (look_r - ROOM_RADIUS).abs() < 1e-3,
                "{s:?} should still look at the unchanged marker radius: {look_r}"
            );
        }
    }

    #[test]
    fn reserved_marker_height_is_a_low_stub_a_third_of_a_station_pylon() {
        assert!(
            (MARKER_HEIGHT_RESERVED - MARKER_HEIGHT / 3.0).abs() < 1e-4,
            "reserved marker is roughly a third the height of a built station's pylon"
        );
        assert!(MARKER_HEIGHT_RESERVED < MARKER_HEIGHT, "still shorter than a station pylon");
    }

    #[test]
    fn every_camera_pose_stays_inside_the_vault_dome() {
        // Outside the dome the camera would face its near inner wall, occluding
        // the room. Every focus (overview + each bearing) must orbit within it.
        for s in Station::ALL {
            let (pos, _) = desired_camera(s);
            assert!(
                pos.length() < DOME_RADIUS,
                "{s:?} camera at {} escapes the dome ({DOME_RADIUS})",
                pos.length()
            );
        }
    }

    // ── circuit-board routes (real production config) ──

    #[test]
    fn w_bundle_terminal_pads_cluster_near_the_dais_foot() {
        // The wiring must visibly flow INTO the station: pads land just past
        // the dais foot radius, well short of the old wall-radius pad range.
        let w_bundle = &route_bundles()[0];
        assert!(
            (w_bundle.pad_range.0 - (palette::STATION_W_DAIS_R + 120.0)).abs() < 1e-3,
            "W pad range should start just past the dais foot: {:?}",
            w_bundle.pad_range
        );
        assert!(
            w_bundle.pad_range.1 < ROOM_RADIUS,
            "W pads should stop well short of the wall, clustering near the station: {:?}",
            w_bundle.pad_range
        );
        assert!(
            w_bundle.pad_range.0 > KEEPOUT_RADIUS,
            "even the nearest W pad clears the console keep-out"
        );
    }

    #[test]
    fn route_bundle_total_count_is_within_the_target_board_density() {
        let total: usize = route_bundles().iter().map(|b| b.count).sum();
        assert!(
            (24..=36).contains(&total),
            "board density should read dense like concept 06: {total} routes"
        );
    }

    #[test]
    fn every_route_bundle_departs_from_outside_the_console_keepout() {
        assert!(RING_OUTER_R > KEEPOUT_RADIUS, "the ring itself clears the keep-out");
        assert!(
            RING_INNER_R - RING_BAND_WIDTH * 0.5 > KEEPOUT_RADIUS,
            "even the inner ring's inner edge clears the keep-out"
        );
    }

    #[test]
    fn every_generated_board_route_clears_the_console_keepout() {
        // The concrete lock the old `TRACE_ARCS` debug_assert covered, applied
        // to the real bundle table + the real ring/chamfer/pad constants
        // `spawn_floor` calls `expand_bundle` with.
        for (bi, bundle) in route_bundles().iter().enumerate() {
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
    fn every_generated_board_route_stays_ldr_including_its_pad() {
        for (bi, bundle) in route_bundles().iter().enumerate() {
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
                let color = [
                    route.hue[0] * route.brightness_scale,
                    route.hue[1] * route.brightness_scale,
                    route.hue[2] * route.brightness_scale,
                ];
                assert!(color.iter().all(|c| *c < 1.0), "bundle {bi} trace must stay LDR: {color:?}");
                let pad = color.map(|c| c * ROUTE_PAD_BRIGHTNESS_GAIN);
                assert!(pad.iter().all(|c| *c < 1.0), "bundle {bi} pad must stay LDR: {pad:?}");
            }
        }
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

    #[test]
    fn console_rings_float_above_the_tabletop() {
        for (y, _, _) in CONSOLE_RINGS {
            assert!(y + TABLE_TOP_Y > TABLE_TOP_Y, "every ring sits above the table's top face");
        }
    }

    // ── the W dais ──

    #[test]
    fn the_dais_body_has_a_positive_height_above_its_foot() {
        // spawn_w_dais builds the body cylinder as
        // (STATION_W_DAIS_TOP_Y - W_DAIS_FOOT_HEIGHT) tall — a palette tweak
        // that let the foot swallow the whole dais height would spawn a
        // zero/negative-height cylinder.
        let body_height = palette::STATION_W_DAIS_TOP_Y - W_DAIS_FOOT_HEIGHT;
        assert!(body_height > 0.0, "dais body must stand above its own foot: {body_height}");
    }

    #[test]
    fn the_dais_foot_is_wider_than_the_dais_top() {
        assert!(
            W_DAIS_FOOT_R > palette::STATION_W_DAIS_R,
            "the foot should ground the dais the way the table plinth grounds the table"
        );
    }

    // ── octagon wall shell (room-level constants) ──

    #[test]
    fn wall_apothem_clears_the_old_radiator_radius_and_the_marker_radius() {
        assert!(WALL_APOTHEM > ROOM_RADIUS, "the shell must enclose the wall stations: {WALL_APOTHEM}");
        assert!(WALL_APOTHEM > 660.0, "the shell must enclose the old radiator radius (660)");
        assert!(WALL_APOTHEM < FLOOR_RADIUS, "the shell must stand on the floor disc");
    }

    #[test]
    fn wall_panel_width_stays_positive_after_the_mullion_gap() {
        let width = bearing::octagon_panel_width(WALL_APOTHEM) - WALL_PANEL_GAP;
        assert!(width > 0.0, "the mullion gap must not eat the whole panel: {width}");
    }

    #[test]
    fn wall_trim_brightness_stays_in_the_restrained_neon_range() {
        // Mission spec: "LDR ~0.5-0.7 of hue" — restrained neon, not a blown
        // highlight, and never HDR (decoration stays < 1.0).
        assert!((0.5..=0.7).contains(&WALL_TRIM_LDR), "trim LDR out of the restrained range: {WALL_TRIM_LDR}");
    }

    #[test]
    fn wall_base_and_mullion_colours_stay_ldr() {
        let lum = |c: [f32; 3]| c[0] + c[1] + c[2];
        assert!(lum(WALL_BASE_COLOR) < 1.0, "panel base must stay LDR (decoration, not live activity)");
        assert!(lum(WALL_MULLION_COLOR) < 1.0, "mullion must stay LDR");
    }

    #[test]
    fn wall_base_is_a_hair_lighter_than_the_dome_rim() {
        let dome_rim = bearing::dome_color(0.0);
        let lum = |c: [f32; 3]| c[0] + c[1] + c[2];
        assert!(
            lum(WALL_BASE_COLOR) > dome_rim[0] + dome_rim[1] + dome_rim[2],
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
    fn ribbon_mesh_has_matching_position_and_normal_counts() {
        let line = [[0.0, 0.0, 0.0], [50.0, 0.0, 0.0], [100.0, 0.0, 0.0]];
        let mesh = ribbon_mesh(&line, 8.0);
        use bevy::mesh::VertexAttributeValues;
        let pos = mesh.attribute(Mesh::ATTRIBUTE_POSITION).unwrap().len();
        let nrm = mesh.attribute(Mesh::ATTRIBUTE_NORMAL).unwrap().len();
        assert_eq!(pos, 6, "2 verts × 3 points");
        assert_eq!(nrm, pos, "one up-normal per vertex");
        if let Some(VertexAttributeValues::Float32x3(ns)) = mesh.attribute(Mesh::ATTRIBUTE_NORMAL) {
            assert!(ns.iter().all(|n| *n == [0.0, 1.0, 0.0]), "all up");
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
