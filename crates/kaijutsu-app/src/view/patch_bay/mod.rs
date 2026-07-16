//! Patch bay station — the circle scene, slice 0 (`docs/scenes/patchbay.md`).
//!
//! Read-only observed reality: the local ALSA seq graph rendered as a round
//! table — brass socket pegs seated around the rim grouped by client, chords
//! of emissive light bowing around the open center for every live
//! subscription. Polled every couple of seconds; hand-run `aconnect` changes
//! appear on the next poll. No write path of any kind (patching stays
//! CLI-only for a long time — the scene is a viewer).
//!
//! **The circle IS the west station** (`docs/scenes/shell.md`, Tardis reading
//! #3 + slice B: "one shared scene graph"; Amy, 2026-07-10 — the wheel stands
//! in for the sign and pylon a station used to need). The whole subtree rides
//! ONE Amy-tunable placement transform ([`STATION_W_PLACEMENT`]) that MOUNTS
//! it on the room's W wall panel, re-oriented face-out by a pitch+yaw
//! composition — internal coordinates are untouched. It is spawned when the
//! room spawns ([`spawn_furniture`], called from `room::enter_room`) and
//! lives as long as the room; diving is a
//! *continuous camera descent* onto it, never a despawn/respawn scene cut. At
//! room scale the chords are the W ambient; the dived view earns its focus by
//! hiding room distractions and showing the label/tick/card LOD ([`PatchBayLod`]).
//!
//! Keys (dived only): Left/Right cycle the selected wire (the inspection plate
//! follows), Up or Esc surfaces to the room, `r` forces a poll.

pub mod geometry;

use bevy::prelude::*;

use crate::midi::RenderPortTraffic;
use crate::patch_graph::{EndpointInfo, PatchGraphReader, PatchGraphSnapshot, diff, without_plumbing};
use crate::shaders::{ChordMaterial, WellCardMaterial};
use crate::text::ShapingFonts;
use crate::text::components::bevy_color_to_brush;
use crate::text::msdf::{FontDataMap, MsdfAtlas, MsdfBlockGlyphs, PositionedGlyph, collect_msdf_glyphs};
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};
use crate::ui::screen::Screen;
use crate::view::palette;
use crate::view::room::nav::Station;
use crate::view::room::{
    PLATE_FONT_SIZE, PLATE_PAD, PLATE_TEX_H, PLATE_TEX_W, RoomState, layout_plate_text,
};
use crate::view::scene_palette::{ScenePalette, lin, lin_scaled};
use crate::view::time_well::panel::{commit_panel_glyphs, create_msdf_panel};
use geometry::{chord_points, layout_sockets};

// ── Station placement seam (Amy-tunable — the ONE knob) ──────────────────────
// Where the patch-bay wheel stands as the west station itself — MOUNTED ON
// the W wall panel now, part of the wall rather than furniture standing in
// front of it (Amy, 2026-07-10: "the surface gets taken over by its
// content"; studio patch bays are wall panels; concept 06 draws the W
// station wall-mounted with threads dropping into the floor traces). This is
// the single seam the slice-B decision (`docs/scenes/shell.md`, open
// question 3, DECIDED 2026-07-09) rides on: the whole `PatchBayRoot` subtree
// is a child of a placement entity carrying this transform, so all
// positioning/scaling/orienting happens HERE and the patch bay's internal
// coordinates never change. `room::spawn_walls` builds the W panel itself
// (the chamber's architecture, not this station's furniture); this placement
// reads the SAME `palette::STATION_W_*`/`WALL_APOTHEM` numbers to seat the
// wheel flush against it (`palette.rs`'s "Station W contract" — room and
// patch bay agree there, never by eyeballing each other).
//
// The dive camera is NO LONGER derived from this placement (2026-07-10
// evening, the fullscreen-panel pivot): `room::fullscreen_pose` computes the
// zoomed pose straight from `palette::WALL_APOTHEM`/`room::WALL_HEIGHT` and
// the station's own bearing direction, independent of this transform. Editing
// `STATION_W_PLACEMENT` moves the wheel itself but no longer moves the
// camera — the two were deliberately decoupled once the camera pose stopped
// needing to know anything about the wheel's own local geometry (`PB_CAM_POS`/
// `PB_CAM_LOOK`/`dive_camera_pose`, all deleted this slice).

/// A rigid-plus-uniform-scale placement of a scene into room space.
struct StationPlacement {
    /// Room-space translation of the scene's local origin.
    translation: Vec3,
    /// Uniform scale applied to the scene's local coordinates.
    scale: f32,
    /// Pitch about local +X (radians), applied BEFORE yaw
    /// ([`placement_rotation`]: `Ry(yaw) * Rx(pitch)`) — stands the wheel up
    /// off its original horizontal orientation so it can hang on a vertical
    /// wall. [`STATION_W_PLACEMENT`]'s doc derives why W's value is
    /// `-FRAC_PI_2`.
    pitch: f32,
    /// Yaw about world +Y (radians), applied AFTER pitch. No longer a
    /// standalone horizontal turn now that W mounts vertically — composed
    /// WITH `pitch` to complete the change of basis; see
    /// [`STATION_W_PLACEMENT`]'s doc.
    yaw: f32,
}

/// The pure rotation half of a placement: pitch about local +X, then yaw
/// about world +Y (`Ry(yaw) * Rx(pitch)`, pitch applied first — Bevy/glam
/// quaternion composition applies the right-hand factor to the vector
/// first). [`placement_transform`] builds the subtree's spawn transform on
/// this; the test-only [`placement_to_room`] below builds the same
/// derivation's checks on it too, so the tests can never drift from what
/// actually gets spawned.
fn placement_rotation(p: &StationPlacement) -> Quat {
    Quat::from_rotation_y(p.yaw) * Quat::from_rotation_x(p.pitch)
}

/// The patch bay's placement at W — mounted ON the wall panel (Amy,
/// 2026-07-10: the wheel is the west station's wall instrument now, not a
/// tabletop on a dais; supersedes the 2026-07-09/-10 dais placement —
/// `STATION_W_X`/`_DAIS_TOP_Y`/`_DAIS_R`, all deleted from `palette.rs`).
///
/// **The rotation.** The standalone scene is a horizontal table: local +Y is
/// the table's upward normal (everything the wheel draws — sockets, chords,
/// labels — sits at local y ≥ 0; only the table's own solid thickness,
/// `TABLE_DEPTH`, occupies y < 0). Mounted on the wall, that normal must
/// point OUT of the wall and INTO the room: world +X, since the W panel
/// faces the room from −X. [`placement_rotation`]'s composition
/// (`Ry(yaw) * Rx(pitch)`) sends local Y to
/// `(sin(pitch)·sin(yaw), cos(pitch), sin(pitch)·cos(yaw))` — wanting
/// `(1, 0, 0)` forces `cos(pitch) = 0` (pitch = ±π/2) and then
/// `cos(yaw) = 0` too (yaw = ±π/2): two candidate pairs, both `+π/2` or both
/// `−π/2`.
///
/// **The roll** (which candidate): the standalone scene's local +Z was the
/// camera-side edge of the table (the old approach camera, before the
/// wall-mount, studied it from +Z). Mounted on the wall, that edge should
/// read as the TOP of the instrument's face, never its bottom — local Z ↦ world Y,
/// not world −Y. Working the same composition through both candidates:
/// `(pitch, yaw) = (+π/2, +π/2)` sends local Z to world −Y (upside down);
/// `(−π/2, −π/2)` sends it to world +Y (right side up) — the tie-breaker.
/// Locked ±1e-5 by the `placement_*` tests below.
///
/// **The placement.** `translation.x` sets the tabletop plane (the
/// placement's local origin) [`palette::STATION_W_PROUD`] world-units proud
/// of the panel ([`palette::WALL_APOTHEM`] out); `translation.y` centers it
/// on the panel ([`palette::STATION_W_MOUNT_Y`], the panel's own vertical
/// center). No thickness lift (the old dais needed one —
/// `palette::STATION_W_PROUD`'s doc has why the wall-mount doesn't): the
/// table's solid backing extrudes local −Y (`TABLE_DEPTH`), which now maps
/// to world −X — toward and through the invisible, single-sided far side of
/// the panel, not onto a load-bearing surface whose top face the table's
/// underside had to land on exactly.
const STATION_W_PLACEMENT: StationPlacement = StationPlacement {
    translation: Vec3::new(
        -(palette::WALL_APOTHEM - palette::STATION_W_PROUD),
        palette::STATION_W_MOUNT_Y,
        0.0,
    ),
    scale: palette::STATION_W_SCALE,
    pitch: -std::f32::consts::FRAC_PI_2,
    yaw: -std::f32::consts::FRAC_PI_2,
};

// ── Scene constants (Amy-tunable) ───────────────────────────────────────────

/// Rim radius where sockets seat; the open center the chords bow around.
const RIM_R: f32 = 300.0;
const HOLE_R: f32 = 95.0;
/// Chord arc lift at midpoint, and samples per chord.
const CHORD_LIFT: f32 = 46.0;
const CHORD_SAMPLES: usize = 32;
const CHORD_WIDTH: f32 = 5.0;
const CHORD_WIDTH_SELECTED: f32 = 8.0;

/// Table annulus (flat, y = 0 top face).
const TABLE_INNER_R: f32 = HOLE_R * 0.92;
const TABLE_OUTER_R: f32 = RIM_R * 1.16;
const TABLE_DEPTH: f32 = 12.0;


// Wire hue — crimson = MIDI fabric (`docs/scenes/patchbay.md`, wire grammar) —
// moved onto `ScenePalette::wire` (normalized identity) × `ScenePalette::
// gain_wire` (the resting HDR lift): `wire * gain_wire` reproduces the old
// pre-multiplied `WIRE_HUE = LinearRgba::rgb(1.4, 0.16, 0.24)` exactly. The
// idle-glow multiplier for the inspected chord (a brighter, bloomier wire)
// moved onto `ScenePalette::gain_chord_selected`.

// ── Live layer (Amy-tunable) ────────────────────────────────────────────────
// Traffic pulses ride the chord src→dest when the render port sends MIDI. The
// packet is animated on the GPU against `globals.time` (one uniform write per
// pulse, `chord.wgsl`); `pulse_band` below is the CPU mirror of that math.

/// Seconds a traffic packet takes to travel a chord source→dest.
const PULSE_TRAVEL_SECS: f32 = 0.42;
/// Gaussian half-width of the packet in length-UV (0..1 across the chord).
const PULSE_BAND_WIDTH: f32 = 0.16;
// Peak brightness added at the packet crest (HDR → bloom = "live action")
// moved onto `ScenePalette::gain_pulse`.
/// A `pulse_time` sentinel far in the past: no packet, wire solid-lit. Every
/// wire the app can't observe stays here (the seam slice 4 fills kernel-ward).
const PULSE_IDLE: f32 = -1.0e6;

/// The inspection card floats this far above the selected chord's apex.
const INFO_PLATE_LIFT: f32 = 58.0;
/// The empty-state (no wires) inspection-plate pose — the original edge
/// placement, kept only for "NO WIRES" / "NO ALSA GRAPH".
const INFO_EDGE_POS: Vec3 = Vec3::new(TABLE_OUTER_R * 0.78, 190.0, TABLE_OUTER_R * 0.35);

/// The app's own render endpoint identity (must match `MidiOut::open` in
/// `midi.rs`): the source of any chord the app can pulse from its own traffic.
const RENDER_CLIENT_NAME: &str = "kaijutsu-app";
const RENDER_PORT_NAME: &str = "render";

/// Poll cadence for the observed graph.
const POLL_SECS: f32 = 2.0;

// ── Etched instrument face (Amy-tunable; strictly LDR — etching never blooms) ─

/// The etched gold ring/tick geometry lies this far above the table's top face
/// (a small +Y like the chords, to clear z-fighting with the flat annulus).
const ETCH_Y: f32 = 0.8;
/// Concentric guide-ring radii (world units): a few rings between the center
/// hole and the rim, plus the ring AT the socket-seat radius (`RIM_R`) the
/// pegs sit on — "legible like a well-designed instrument" (mockup 14).
const ETCH_RING_RADII: [f32; 4] = [150.0, 205.0, 260.0, RIM_R];
const ETCH_RING_WIDTH: f32 = 2.0;
/// Radial seat tick: a short mark reaching inward from the rim at each socket
/// angle (the seats come from `layout_sockets`, so ticks rebuild with them).
const ETCH_TICK_LEN: f32 = 20.0;
const ETCH_TICK_WIDTH: f32 = 2.4;

// ── Rim text layers ──────────────────────────────────────────────────────────

/// Per-socket port labels — the PRIMARY rim text: bright, inner, floating just
/// above each peg (`docs/scenes/patchbay.md` socket grammar: RENDER, SYNTH…).
/// Amy-tunable: nudged out from the pre-wall-mount `RIM_R * 1.02` (2026-07-10,
/// wall-mount text fix) — every label now shares ONE fixed rotation
/// ([`upright_wall_facing`]) instead of a per-seat camera billboard, so its
/// width runs along the SAME world axis at every seat instead of always
/// tangent to the rim. At the two seats where that shared axis happens to
/// line up with the radius rather than the rim's tangent, a label centered
/// right at the rim would hang roughly half its width back over the pegs and
/// etch ticks; parking it a label-half-width-plus-margin past `RIM_R` clears
/// that worst case (`PORT_LABEL_W`'s own half-width folds straight into the
/// constant so the two stay in lockstep). `GROUP_PLATE_R` below already
/// cleared this same worst case at its old value, so it's untouched — only
/// its margin against this now-wider ring is a residual, seat-dependent risk
/// worth a visual check.
const PORT_LABEL_R: f32 = RIM_R + PORT_LABEL_W / 2.0 + 20.0;
/// How far this floats proud of the wall face (local Y — the wall's own
/// depth axis once mounted, [`STATION_W_PLACEMENT`]'s doc). Raised from 50.0
/// pre-mount so it never sits flush with the group nameplate at the same
/// depth.
const PORT_LABEL_Y: f32 = 64.0;
const PORT_LABEL_W: f32 = 108.0;
/// Height keeps the shared plate texture's aspect so the glyphs don't stretch.
const PORT_LABEL_H: f32 =
    PORT_LABEL_W * crate::view::room::PLATE_TEX_H / crate::view::room::PLATE_TEX_W;
const PORT_LABEL_DIM: f32 = 1.0;
/// Client group nameplates — the SUPPORTING layer: dimmer and further out than
/// the port labels, so the two text tiers read as hierarchy, not noise.
const GROUP_PLATE_W: f32 = 150.0;
const GROUP_PLATE_H: f32 = 44.0;
/// Pushed out (1.22 → 1.32, pre-wall-mount) and never re-tuned for the
/// wall-mount text fix: at 1.32 it already clears `RIM_R` by more than its own
/// half-width even at a worst-case radial-aligned seat, the same bar
/// `PORT_LABEL_R` above was nudged to meet.
const GROUP_PLATE_R: f32 = RIM_R * 1.32;
const GROUP_PLATE_Y: f32 = 16.0;
const GROUP_PLATE_DIM: f32 = 0.5;

// ── Placement math (pure; the room-furniture seam) ───────────────────────────

/// The `Transform` for the placement entity that re-roots the whole patch-bay
/// subtree into room space — translation, the pitch+yaw rotation
/// ([`placement_rotation`]), and uniform scale. `room::enter_room` hangs
/// `PatchBayRoot` under an entity carrying this.
fn placement_transform(p: &StationPlacement) -> Transform {
    Transform::from_translation(p.translation)
        .with_rotation(placement_rotation(p))
        .with_scale(Vec3::splat(p.scale))
}

/// Map a patch-bay-LOCAL point to room space through a placement — the same
/// similarity transform [`placement_transform`] applies to the subtree, but as
/// a point mapping a test can check without spawning an entity. Test-only:
/// the fullscreen dive camera no longer reads this placement at all
/// (`room::fullscreen_pose` computes it straight from the station's bearing
/// and the shared wall constants, decoupled from the wheel's own local
/// geometry — 2026-07-10 evening, the fullscreen-panel pivot); this helper
/// survives purely to let the `placement_*` tests below check the rotation
/// derivation against concrete points instead of raw quaternion algebra.
#[cfg(test)]
fn placement_to_room(p: &StationPlacement, local: Vec3) -> Vec3 {
    p.translation + placement_rotation(p) * (local * p.scale)
}

// ── State ───────────────────────────────────────────────────────────────────

/// Main-thread-only ALSA handle (the `Seq` is not Send — same NonSend stance
/// as `MidiSink`). `None` until the first enter; `Some(None)` = open failed
/// (logged once; the scene shows an empty table rather than crashing —
/// ALSA-less machines still get the room nav).
#[derive(Default)]
pub struct PatchBayAlsa {
    reader: Option<Option<PatchGraphReader>>,
}

impl PatchBayAlsa {
    /// Clear a latched failed open (`Some(None)`) so the next poll retries
    /// `PatchGraphReader::open()`. A healthy reader (`Some(Some(_))`) is left
    /// alone — closing and reopening a working handle would churn its ALSA
    /// client id for no reason. `None` (never opened) is already a no-op.
    fn clear_failed_open(&mut self) {
        if matches!(self.reader, Some(None)) {
            self.reader = None;
        }
    }
}

#[derive(Resource)]
pub struct PatchBayState {
    pub snapshot: PatchGraphSnapshot,
    pub selected: usize,
    /// Rebuild the socket/chord entities on the next frame.
    scene_dirty: bool,
    /// Re-lay-out the text plates on the next frame.
    text_dirty: bool,
    timer: Timer,
}

impl Default for PatchBayState {
    fn default() -> Self {
        Self {
            snapshot: PatchGraphSnapshot::default(),
            selected: 0,
            scene_dirty: false,
            text_dirty: true,
            timer: Timer::from_seconds(POLL_SECS, TimerMode::Repeating),
        }
    }
}

impl PatchBayState {
    /// Arm the text layer alone (not the full scene rebuild [`arm_scene`]
    /// forces) — the old `enter_patch_bay`'s one job, called now from the
    /// zoom-in site (`room::room_keyboard`) since there's no more
    /// `OnEnter(Screen::PatchBay)` to hang it on. `text_dirty` is private so
    /// this method, not a raw field write, is the cross-module seam.
    pub(crate) fn arm_text(&mut self) {
        self.text_dirty = true;
    }
}

// ── Components ──────────────────────────────────────────────────────────────

/// The placement entity that re-roots the patch bay into room space at the W
/// bearing (child of `RoomRoot`; carries [`STATION_W_PLACEMENT`]). `PatchBayRoot`
/// is its child, so the whole scene moves as one.
#[derive(Component)]
pub struct StationWPlacement;

#[derive(Component)]
pub struct PatchBayRoot;

/// The zoomed view's LOD layer — port labels, radial etch ticks, group/title/info
/// plates: the "dedicated view" text+tick detail that appears only when zoomed
/// (`docs/scenes/shell.md`, slice B). `apply_patch_lod` shows these while
/// [`patch_bay_zoomed`] and hides them at room scale, where the bare chords over
/// the socket rings are the W ambient. Spawned `Visibility::Hidden` so the
/// room-scale default is correct with no first-frame flash.
#[derive(Component)]
pub struct PatchBayLod;

/// A chord entity; the index into `PatchBayState.snapshot.wires`.
#[derive(Component)]
pub struct ChordWire(pub usize);

/// A chord whose source IS the app's render port — the only wires the app can
/// observe traffic on (edge reality, `docs/scenes/patchbay.md`). These pulse when
/// MIDI flows; everything else stays solid-lit.
#[derive(Component)]
pub struct RenderChord;

#[derive(Component)]
pub struct SocketPeg;

/// A client-group nameplate around the rim.
#[derive(Component)]
pub struct GroupPlate(pub String);

/// The selected wire's inspection plate.
#[derive(Component)]
pub struct InfoPlate;

/// Static title / legend plates (filled once).
#[derive(Component)]
pub struct TitlePlate(pub &'static str);

/// A floating ALL-CAPS holographic label above one socket peg. Holds the
/// derived display string; `fill_port_labels` commits its glyphs once the font
/// loads (the same async-font gate the other plates use).
#[derive(Component)]
pub struct PortLabel(String);

/// A radial etch tick at a socket seat angle. Seat-driven, so it rebuilds with
/// the sockets (unlike the static concentric rings spawned once in `spawn_furniture`).
#[derive(Component)]
pub struct EtchTick;

// ── Plugin ──────────────────────────────────────────────────────────────────

pub struct PatchBayPlugin;

impl Plugin for PatchBayPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PatchBayState>()
            .insert_non_send_resource(PatchBayAlsa::default())
            .add_plugins(MaterialPlugin::<ChordMaterial>::default())
            // No `OnEnter`/`OnExit(Screen::PatchBay)` any more (2026-07-10
            // evening, the fullscreen-panel pivot): there is no second screen
            // to hang them on. The old `enter_patch_bay`'s one job — arming
            // `text_dirty` on the way in — moved to the zoom-in site
            // (`room::room_keyboard`); the old `exit_patch_bay`'s teardown
            // duty was already `room::teardown_room`, which `room::exit_room`
            // now runs unconditionally on its own.
            .add_systems(
                Update,
                (
                    // Dived-only input; first so a Left/Right selection lands
                    // before the rebuild/fill it feeds. Ordering vs
                    // `room_keyboard` is no longer load-bearing — ActionFired
                    // carries its binding context, so a PatchBayZoomed Esc
                    // can't replay through the room's consumer. Runs after
                    // the central dispatcher to see this frame's actions.
                    patch_bay_keyboard
                        .run_if(patch_bay_zoomed)
                        .after(crate::input::InputPhase::Dispatch),
                    // Ambient truth: the observed graph, its chords, the
                    // selection glow, and traffic pulses stay live at room
                    // scale AND zoomed — the chords ARE the W ambient even
                    // seen from room scale (the bearing's promise), so these
                    // can't be gated behind the zoom. `Screen::Room` is now
                    // the only screen this scene graph occupies at all.
                    poll_patch_graph.run_if(in_state(Screen::Room)),
                    rebuild_patch_scene.run_if(in_state(Screen::Room)),
                    update_wire_selection.run_if(in_state(Screen::Room)),
                    // Zoomed-only detail: the inspection card pose and the text
                    // layer (fills after rebuild spawns the plates it reads;
                    // `fill_patch_text` owns the `text_dirty` clear, so port
                    // labels fill first on the same armed flag).
                    position_info_plate.run_if(patch_bay_zoomed),
                    fill_port_labels.run_if(patch_bay_zoomed),
                    fill_patch_text.run_if(patch_bay_zoomed),
                    pulse_render_chords.run_if(in_state(Screen::Room)),
                    // LOD visibility last, so a label/tick rebuild spawned this
                    // frame gets its room-vs-zoom visibility set before render.
                    apply_patch_lod.run_if(in_state(Screen::Room)),
                )
                    .chain(),
            );
    }
}

// ── Zoom gate (2026-07-10 evening, the fullscreen-panel pivot) ──────────────

/// Pure predicate: is `station` the room's current zoom target? The Bevy
/// system-param wrapper below ([`patch_bay_zoomed`]) is what the plugin
/// actually registers as a `run_if` condition; this is its testable core.
fn is_zoomed_into(zoomed: Option<Station>, station: Station) -> bool {
    zoomed == Some(station)
}

/// The `run_if` condition every dived-only patch-bay system gates on now —
/// the direct replacement for `in_state(Screen::PatchBay)`: diving is a
/// `RoomState::zoomed` write, not a screen, so the gate reads the resource
/// instead. `RoomState::zoomed` can only ever be `Some(PatchBay)` while
/// actually inside `Screen::Room` (`room::room_keyboard` sets it, and
/// `room::exit_room` unconditionally clears it on the way out), so checking
/// `zoomed` alone is sufficient — no separate `in_state(Screen::Room)` guard
/// needed alongside it.
fn patch_bay_zoomed(room: Res<RoomState>) -> bool {
    is_zoomed_into(room.zoomed, Station::PatchBay)
}

// ── Scene lifecycle ──────────────────────────────────────────────────────────

/// Arm a fresh poll + full scene/text rebuild for the next frame. `PatchBayState`
/// (and its `snapshot`) is a `Resource` — it outlives the entities `RoomRoot`
/// carries. Called from `room::enter_room` when the room (and the W furniture)
/// first spawns, so the observed graph is polled and its chords built straight
/// away. Without forcing `scene_dirty`, a room re-entry where the ALSA graph
/// hasn't changed since last time produces an empty `diff` in `poll_patch_graph`
/// and `rebuild_patch_scene` never runs — a bare table forever, even though
/// `state.snapshot` is valid.
pub(crate) fn arm_scene(state: &mut PatchBayState) {
    let full = state.timer.duration();
    state.timer.set_elapsed(full);
    state.text_dirty = true;
    state.scene_dirty = true;
}

/// Spawn the patch-bay wheel as **the west station itself** — the static half
/// of the scene (the placement anchor, the table, the etched guide rings, and
/// the title/legend/inspection plates). The seat-driven sockets, ticks,
/// chords, and per-client plates are rebuilt from the observed graph by
/// `rebuild_patch_scene`. Called once from `room::enter_room`; the whole
/// subtree lives as long as `RoomRoot`.
pub(crate) fn spawn_furniture(
    commands: &mut Commands,
    room_root: Entity,
    palette: &ScenePalette,
    meshes: &mut Assets<Mesh>,
    std_materials: &mut Assets<StandardMaterial>,
    card_materials: &mut Assets<WellCardMaterial>,
    images: &mut Assets<Image>,
) {
    // Re-root the whole circle into room space at W: the placement entity holds
    // the ONE Amy-tunable transform, `PatchBayRoot` rides under it, and every
    // child below keeps its untouched local coordinates (the seam).
    let placement = commands
        .spawn((
            StationWPlacement,
            placement_transform(&STATION_W_PLACEMENT),
            Visibility::Inherited,
            Name::new("StationWPlacement"),
            ChildOf(room_root),
        ))
        .id();
    let root = commands
        .spawn((
            PatchBayRoot,
            Transform::default(),
            Visibility::Inherited,
            Name::new("PatchBayRoot"),
            ChildOf(placement),
        ))
        .id();

    // The table: a flat annulus — the hole in the middle IS the open-center
    // rule, built into the furniture. Extrusions extrude along Z; rotate to
    // lie flat with the top face up.
    let table_mesh = meshes.add(Extrusion::new(Annulus::new(TABLE_INNER_R, TABLE_OUTER_R), TABLE_DEPTH));
    // Unlit, palette-driven (Amy, 2026-07-10 — "unify materials with the
    // room"): `ScenePalette::dark_surface` is one shade up from the room's
    // own furniture surface, so the instrument's working face reads against
    // the wall panel behind it with no lamp needed.
    let table_material = std_materials.add(StandardMaterial {
        base_color: lin(palette.dark_surface),
        unlit: true,
        ..default()
    });
    commands.spawn((
        Mesh3d(table_mesh),
        MeshMaterial3d(table_material),
        Transform::from_translation(Vec3::new(0.0, -TABLE_DEPTH / 2.0, 0.0))
            .with_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
        Visibility::Inherited,
        Name::new("PatchBayTable"),
        ChildOf(root),
    ));

    // Etched instrument face: faint gold concentric guide rings lying just
    // above the table's top face — the static half of mockup 14's "concentric
    // guide rings and radial ticks etched in faint gold." The per-seat radial
    // ticks are seat-driven and spawn with the sockets in `rebuild_patch_scene`.
    // One shared unlit material, `ScenePalette::gold` at the etch tier (dimmer
    // than trim so etched detail supports rather than competes) — reads at
    // rest and never blooms; `cull_mode: None` keeps the up-facing annulus
    // visible either side.
    let etch_material = std_materials.add(StandardMaterial {
        base_color: lin_scaled(palette.gold, palette.etch),
        unlit: true,
        cull_mode: None,
        ..default()
    });
    for &r in &ETCH_RING_RADII {
        let ring = meshes.add(Annulus::new(r - ETCH_RING_WIDTH * 0.5, r + ETCH_RING_WIDTH * 0.5));
        commands.spawn((
            Mesh3d(ring),
            MeshMaterial3d(etch_material.clone()),
            // Annulus lies in XY facing +Z; rotate it flat so its face points up.
            Transform::from_xyz(0.0, ETCH_Y, 0.0)
                .with_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
            Visibility::Inherited,
            Name::new("PatchBayEtchRing"),
            ChildOf(root),
        ));
    }

    // Title, legend, inspection plates (MSDF; filled by `fill_patch_text` only
    // while dived). All three are the dive LOD — tagged `PatchBayLod` and spawned
    // hidden by `plate_bundle`, so the room-scale ambient stays text-free.
    let title = plate_bundle(
        meshes,
        card_materials,
        images,
        Vec3::new(0.0, 150.0, -TABLE_OUTER_R - 60.0),
        1.4,
    );
    commands.spawn((
        TitlePlate("PATCH BAY"),
        PatchBayLod,
        title,
        Name::new("PatchBayTitle"),
        ChildOf(root),
    ));

    let legend = plate_bundle(
        meshes,
        card_materials,
        images,
        Vec3::new(0.0, 8.0, TABLE_OUTER_R + 90.0),
        0.9,
    );
    commands.spawn((
        TitlePlate("<- -> WIRE   UP/ESC ROOM   R RESCAN"),
        PatchBayLod,
        legend,
        Name::new("PatchBayLegend"),
        ChildOf(root),
    ));

    // Spawn the inspection plate at the empty-state edge pose; `position_info_plate`
    // blooms it onto the selected chord's apex once a wire is selected.
    let info = plate_bundle(meshes, card_materials, images, INFO_EDGE_POS, 1.2);
    commands.spawn((
        InfoPlate,
        PatchBayLod,
        info,
        Name::new("PatchBayInfo"),
        ChildOf(root),
    ));

    info!("patch-bay: stationed as the west station");
}

/// The zoom LOD: show the label/tick/card layer while [`patch_bay_zoomed`],
/// hide it at room scale (`docs/scenes/shell.md`, slice B — the zoomed view
/// earns its focus by *showing the labels*, the room ambient is the bare
/// chords over the socket rings). Change-guarded so a settled entity never
/// re-dirties; it also corrects the labels/ticks `rebuild_patch_scene` spawns
/// this frame (they spawn `Hidden`, so the room-scale default needs no fix —
/// only the zoom shows them).
fn apply_patch_lod(room: Res<RoomState>, mut lod: Query<&mut Visibility, With<PatchBayLod>>) {
    let want = if is_zoomed_into(room.zoomed, Station::PatchBay) {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    for mut vis in lod.iter_mut() {
        if *vis != want {
            *vis = want;
        }
    }
}

// ── Systems ─────────────────────────────────────────────────────────────────

/// Actions, zoomed-only (`run_if(patch_bay_zoomed)`, bindings in the
/// `PatchBayZoomed` context of `input/defaults.rs`): StepNext/StepPrev
/// (Left/Right/Tab) cycle wires; PopLevel (Up/Esc) surfaces to the room —
/// clearing `RoomState::zoomed` directly, no `Screen` transition
/// (`room::room_keyboard`'s own doc on why it steps back entirely while
/// zoomed and leaves these actions to this system); Rescan (`r`) rescans.
fn patch_bay_keyboard(
    mut actions: MessageReader<crate::input::ActionFired>,
    mut state: ResMut<PatchBayState>,
    mut alsa: NonSendMut<PatchBayAlsa>,
    mut room: ResMut<RoomState>,
) {
    use crate::input::Action;

    for crate::input::ActionFired { action, context } in actions.read() {
        if *context != crate::input::InputContext::PatchBayZoomed {
            continue;
        }
        match action {
            Action::StepNext | Action::StepPrev => {
                let n = state.snapshot.wires.len();
                if n > 0 {
                    let step = if matches!(action, Action::StepNext) { 1 } else { n - 1 };
                    state.selected = (state.selected + step) % n;
                    state.text_dirty = true;
                }
            }
            Action::Rescan => {
                let elapsed = state.timer.duration();
                state.timer.set_elapsed(elapsed);
                // A failed open otherwise latches forever (`poll_patch_graph`'s
                // `get_or_insert_with` never runs its closure again); an explicit
                // rescan is the one path allowed to retry it.
                alsa.clear_failed_open();
            }
            Action::PopLevel => {
                room.zoomed = None;
            }
            _ => {}
        }
    }
}

/// Poll the observed graph on a slow timer; only mark dirty on real change.
fn poll_patch_graph(
    time: Res<Time>,
    mut alsa: NonSendMut<PatchBayAlsa>,
    mut state: ResMut<PatchBayState>,
) {
    if !state.timer.tick(time.delta()).just_finished() {
        return;
    }

    let reader = alsa.reader.get_or_insert_with(|| match PatchGraphReader::open() {
        Ok(r) => Some(r),
        Err(e) => {
            warn!("patch-bay: ALSA unavailable, showing an empty table: {e}");
            None
        }
    });
    let Some(reader) = reader.as_ref() else {
        return;
    };

    match reader.snapshot() {
        Ok(snap) => {
            // `own_client`: the reader's own client — resolve it as the one
            // named "kaijutsu-patchview" (the alsa crate exposes no
            // client_id() on Seq through this path; the name is ours).
            let own = snap
                .endpoints
                .iter()
                .find(|e| e.client_name == "kaijutsu-patchview")
                .map(|e| e.client_id)
                .unwrap_or(-1);
            let filtered = without_plumbing(&snap, own);
            let delta = diff(&state.snapshot, &filtered);
            if !delta.is_empty() {
                info!(
                    "patch-bay: graph changed (+{} / -{} wires{})",
                    delta.added_wires.len(),
                    delta.removed_wires.len(),
                    if delta.endpoints_changed { ", endpoints changed" } else { "" },
                );
                state.selected = state.selected.min(filtered.wires.len().saturating_sub(1));
                state.snapshot = filtered;
                state.scene_dirty = true;
                state.text_dirty = true;
            }
        }
        Err(e) => warn!("patch-bay: snapshot failed: {e}"),
    }
}

/// Rebuild sockets, group plates, and chords from the snapshot.
fn rebuild_patch_scene(
    mut commands: Commands,
    mut state: ResMut<PatchBayState>,
    palette: Res<ScenePalette>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut std_materials: ResMut<Assets<StandardMaterial>>,
    mut card_materials: ResMut<Assets<WellCardMaterial>>,
    mut chord_materials: ResMut<Assets<ChordMaterial>>,
    mut images: ResMut<Assets<Image>>,
    roots: Query<Entity, With<PatchBayRoot>>,
    old: Query<
        Entity,
        Or<(With<ChordWire>, With<SocketPeg>, With<GroupPlate>, With<EtchTick>, With<PortLabel>)>,
    >,
) {
    if !state.scene_dirty {
        return;
    }
    state.scene_dirty = false;
    let Ok(root) = roots.single() else {
        return;
    };
    for e in old.iter() {
        commands.entity(e).despawn();
    }

    // Group consecutive endpoints by client (snapshot is sorted by
    // (client_id, port_id), so runs are exact client groups).
    let mut groups: Vec<(String, usize)> = Vec::new();
    for ep in &state.snapshot.endpoints {
        match groups.last_mut() {
            Some((name, n)) if *name == ep.client_name => *n += 1,
            _ => groups.push((ep.client_name.clone(), 1)),
        }
    }
    let (seats, labels) = layout_sockets(&groups);

    // endpoint index → rim angle.
    let angle_of: Vec<f32> = seats.iter().map(|s| s.angle).collect();

    // First endpoint index of each group (groups are exact consecutive runs
    // over `state.snapshot.endpoints`, so a prefix sum over the port counts
    // lands on it) — used below to find a single-port group's lone endpoint
    // for the nameplate-redundancy check.
    let group_starts: Vec<usize> = {
        let mut cursor = 0usize;
        groups
            .iter()
            .map(|(_, n)| {
                let start = cursor;
                cursor += n;
                start
            })
            .collect()
    };

    // Per-client port counts drive the label heuristic's multi-port fallback
    // (keyed by client_id, the true identity — two clients can share a name).
    let mut port_counts: std::collections::HashMap<i32, usize> = std::collections::HashMap::new();
    for ep in &state.snapshot.endpoints {
        *port_counts.entry(ep.client_id).or_default() += 1;
    }

    // Sockets: brass pegs at each seat, each with its etched radial seat tick
    // and its ALL-CAPS holographic port label floating above.
    let peg_mesh = meshes.add(Cylinder::new(7.0, 12.0));
    let peg_material = std_materials.add(StandardMaterial {
        base_color: lin_scaled(palette.brass, palette.hardware),
        unlit: true,
        ..default()
    });
    let tick_material = std_materials.add(StandardMaterial {
        base_color: lin_scaled(palette.gold, palette.etch),
        unlit: true,
        cull_mode: None,
        ..default()
    });
    for seat in &seats {
        let (s, c) = seat.angle.sin_cos();
        commands.spawn((
            SocketPeg,
            Mesh3d(peg_mesh.clone()),
            MeshMaterial3d(peg_material.clone()),
            Transform::from_translation(Vec3::new(RIM_R * c, 6.0, RIM_R * s)),
            Visibility::Inherited,
            Name::new("SocketPeg"),
            ChildOf(root),
        ));

        // Etched radial tick: a short gold mark reaching inward from the rim at
        // this seat angle (a fresh flat ribbon per seat — cheap at ~10 sockets).
        let tick_in = Vec3::new((RIM_R - ETCH_TICK_LEN) * c, ETCH_Y, (RIM_R - ETCH_TICK_LEN) * s);
        let tick_out = Vec3::new(RIM_R * c, ETCH_Y, RIM_R * s);
        let tick_mesh = meshes.add(ribbon_mesh(&[tick_in.to_array(), tick_out.to_array()], ETCH_TICK_WIDTH));
        commands.spawn((
            EtchTick,
            PatchBayLod,
            Mesh3d(tick_mesh),
            MeshMaterial3d(tick_material.clone()),
            Transform::default(),
            Visibility::Hidden,
            Name::new("EtchTick"),
            ChildOf(root),
        ));

        // Port label: the primary rim text, bright and inner, floating above
        // the peg with the shared upright wall-facing (`upright_wall_facing`),
        // never rotated to its seat angle — reads like a numeral painted
        // upright on a clock face, not laid along the radius. Text is
        // committed once the font loads by `fill_port_labels`.
        if let Some(ep) = state.snapshot.endpoints.get(seat.endpoint_index) {
            let count = port_counts.get(&ep.client_id).copied().unwrap_or(1);
            let text = socket_label(&ep.client_name, &ep.port_name, count, ep.port_id);
            let pos = Vec3::new(PORT_LABEL_R * c, PORT_LABEL_Y, PORT_LABEL_R * s);
            let mesh = meshes.add(Rectangle::new(PORT_LABEL_W, PORT_LABEL_H));
            let (image, panel) = create_msdf_panel(
                &mut images,
                crate::view::room::PLATE_TEX_W as u32,
                crate::view::room::PLATE_TEX_H as u32,
            );
            let material = card_materials.add(WellCardMaterial {
                texture: image,
                accent: Vec4::ZERO,
                params: Vec4::ZERO,
                shape: Vec4::new(
                    crate::view::room::PLATE_TEX_W / crate::view::room::PLATE_TEX_H,
                    0.0,
                    0.0,
                    0.0,
                ),
                border: Vec4::ZERO,
                dim: Vec4::new(PORT_LABEL_DIM, 0.0, 0.0, 0.0),
            });
            commands.spawn((
                PortLabel(text),
                PatchBayLod,
                Mesh3d(mesh),
                MeshMaterial3d(material),
                upright_plate(pos),
                Visibility::Hidden,
                panel,
                Name::new("PortLabel"),
                ChildOf(root),
            ));
        }
    }

    // Group nameplates: the supporting layer — dimmer and further out than the
    // port labels, sharing the SAME upright wall-facing as every other wheel
    // plate (never rotated to its seat angle). `layout_sockets` emits exactly
    // one label per group, same order as `groups`.
    debug_assert_eq!(labels.len(), groups.len(), "layout_sockets: one label per group");
    for (i, label) in labels.iter().enumerate() {
        // A single-port client whose lone port label already reads the same
        // as this nameplate (mod case — e.g. port "MIDI THROUGH" under
        // nameplate "Midi Through") gets no nameplate: it would add nothing
        // over the port label already floating on that one seat.
        if groups[i].1 == 1 {
            let ep = &state.snapshot.endpoints[group_starts[i]];
            let count = port_counts.get(&ep.client_id).copied().unwrap_or(1);
            let port_label = socket_label(&ep.client_name, &ep.port_name, count, ep.port_id);
            if nameplate_redundant(&label.client_name, &port_label) {
                continue;
            }
        }
        let (s, c) = label.angle.sin_cos();
        let pos = Vec3::new(GROUP_PLATE_R * c, GROUP_PLATE_Y, GROUP_PLATE_R * s);
        let mesh = meshes.add(Rectangle::new(GROUP_PLATE_W, GROUP_PLATE_H));
        let (image, panel) = create_msdf_panel(
            &mut images,
            crate::view::room::PLATE_TEX_W as u32,
            crate::view::room::PLATE_TEX_H as u32,
        );
        let material = card_materials.add(WellCardMaterial {
            texture: image,
            accent: Vec4::ZERO,
            params: Vec4::ZERO,
            shape: Vec4::new(
                crate::view::room::PLATE_TEX_W / crate::view::room::PLATE_TEX_H,
                0.0,
                0.0,
                0.0,
            ),
            border: Vec4::ZERO,
            dim: Vec4::new(GROUP_PLATE_DIM, 0.0, 0.0, 0.0),
        });
        commands.spawn((
            GroupPlate(label.client_name.clone()),
            PatchBayLod,
            Mesh3d(mesh),
            MeshMaterial3d(material),
            upright_plate(pos),
            Visibility::Hidden,
            panel,
            Name::new(format!("GroupPlate-{}", label.client_name)),
            ChildOf(root),
        ));
    }

    // Chords: one emissive ribbon per wire, bowing around the hole.
    let by_addr: std::collections::HashMap<(i32, i32), usize> = state
        .snapshot
        .endpoints
        .iter()
        .enumerate()
        .map(|(i, e)| ((e.client_id, e.port_id), i))
        .collect();
    // The wire hue used to be its own pre-multiplied HDR constant
    // (`WIRE_HUE = LinearRgba::rgb(1.4, 0.16, 0.24)`); `wire * gain_wire`
    // reproduces it exactly from the normalized identity hue × its resting
    // gain (`ScenePalette::wire`/`gain_wire`).
    let wire_color = ScenePalette::vec3(palette.wire) * palette.gain_wire;
    for (wi, wire) in state.snapshot.wires.iter().enumerate() {
        let (Some(&si), Some(&di)) = (by_addr.get(&wire.src), by_addr.get(&wire.dst)) else {
            continue; // endpoint filtered away; skip its wire
        };
        let (Some(&a1), Some(&a2)) = (angle_of.get(si), angle_of.get(di)) else {
            continue;
        };
        let points = chord_points(a1, a2, RIM_R, HOLE_R, CHORD_LIFT, CHORD_SAMPLES);
        let selected = wi == state.selected;
        let width = if selected { CHORD_WIDTH_SELECTED } else { CHORD_WIDTH };
        let mesh = meshes.add(ribbon_mesh(&points, width));
        let material = chord_materials.add(ChordMaterial {
            color: wire_color.extend(1.0),
            // params.y = PULSE_IDLE: no packet until the pulse system stamps a
            // send. tune carries the Amy-tunable pulse shape into the shader.
            params: Vec4::new(if selected { 1.0 } else { 0.0 }, PULSE_IDLE, 0.0, 0.0),
            tune: Vec4::new(
                PULSE_TRAVEL_SECS,
                PULSE_BAND_WIDTH,
                palette.gain_pulse,
                palette.gain_chord_selected,
            ),
        });
        let mut chord = commands.spawn((
            ChordWire(wi),
            Mesh3d(mesh),
            MeshMaterial3d(material),
            Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)),
            Visibility::Inherited,
            Name::new(format!("Chord-{wi}")),
            ChildOf(root),
        ));
        // A wire leaving our own render port is the only traffic we can observe:
        // tag it so `pulse_render_chords` lights it when MIDI flows.
        if is_render_port(&state.snapshot.endpoints[si]) {
            chord.insert(RenderChord);
        }
    }
}

/// Cheap selection update between rebuilds: the inspected chord's idle glow
/// (`params.x`) follows `selected`.
fn update_wire_selection(
    state: Res<PatchBayState>,
    mut chord_materials: ResMut<Assets<ChordMaterial>>,
    chords: Query<(&ChordWire, &MeshMaterial3d<ChordMaterial>)>,
) {
    if !state.is_changed() {
        return;
    }
    for (chord, handle) in chords.iter() {
        let want = if chord.0 == state.selected { 1.0 } else { 0.0 };
        // Read non-dirtying; only `get_mut` when the flag actually flips (a
        // Left/Right press touches two chords, not the whole set).
        let Some(cur) = chord_materials.get(&handle.0).map(|m| m.params.x) else {
            continue;
        };
        if cur != want
            && let Some(mat) = chord_materials.get_mut(&handle.0)
        {
            mat.params.x = want;
        }
    }
}

/// The live layer: on a render-port send, restart the traveling packet on every
/// chord the app can observe (`With<RenderChord>`) by stamping `params.y` with
/// the current `globals.time` — one uniform write per pulse; the shader animates
/// the band from there (`chord.wgsl`). A quiet chord is never touched.
fn pulse_render_chords(
    time: Res<Time>,
    mut traffic: MessageReader<RenderPortTraffic>,
    mut chord_materials: ResMut<Assets<ChordMaterial>>,
    chords: Query<&MeshMaterial3d<ChordMaterial>, With<RenderChord>>,
) {
    // Drain the frame; collapse a burst of sends into one packet restart.
    if traffic.read().count() == 0 {
        return;
    }
    let now = time.elapsed_secs_wrapped();
    for handle in chords.iter() {
        if let Some(mat) = chord_materials.get_mut(&handle.0) {
            mat.params.y = now;
        }
    }
}

/// Bloom the inspection card onto the selected chord's apex (its arc midpoint,
/// lifted), with the shared upright wall-facing — recomputed whenever the
/// selection or the graph changes. With no drawable chord (no wires, or an
/// endpoint filtered away) it falls back to the edge pose the "NO WIRES" text
/// lives at.
fn position_info_plate(
    state: Res<PatchBayState>,
    mut plate: Query<&mut Transform, With<InfoPlate>>,
) {
    if !state.is_changed() {
        return;
    }
    let Ok(mut tf) = plate.single_mut() else {
        return;
    };
    let target = selected_chord_apex(&state.snapshot, state.selected)
        .map(|apex| apex + Vec3::Y * INFO_PLATE_LIFT)
        .unwrap_or(INFO_EDGE_POS);
    *tf = upright_plate(target);
}

/// Fill/refresh every text plate when dirty (same async-font gate as the
/// well's label builders). Title/legend are static; the info plate follows
/// the selection; group plates carry their client names.
fn fill_patch_text(
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    mut state: ResMut<PatchBayState>,
    mut titles: Query<(&TitlePlate, &mut MsdfBlockGlyphs), (Without<InfoPlate>, Without<GroupPlate>)>,
    mut info: Query<&mut MsdfBlockGlyphs, (With<InfoPlate>, Without<GroupPlate>)>,
    mut groups: Query<(&GroupPlate, &mut MsdfBlockGlyphs), Without<InfoPlate>>,
) {
    if !state.text_dirty {
        return;
    }
    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };
    let Some(atlas) = atlas.as_deref_mut() else {
        return;
    };

    for (title, mut msdf) in titles.iter_mut() {
        let glyphs = layout_plate_text(title.0, font, atlas, &mut font_data_map);
        commit_panel_glyphs(&mut msdf, glyphs);
    }
    for (plate, mut msdf) in groups.iter_mut() {
        let glyphs = layout_plate_text(&plate.0, font, atlas, &mut font_data_map);
        commit_panel_glyphs(&mut msdf, glyphs);
    }
    if let Ok(mut msdf) = info.single_mut() {
        let text = describe_selection(&state.snapshot, state.selected);
        // Shrink-to-fit (not the fixed-size `layout_plate_text`): a long
        // `client:port -> client:port` used to overflow the plate (recorded in
        // `docs/issues.md`); this steps the font down until the wire name fits.
        let glyphs = layout_info_text(&text, font, atlas, &mut font_data_map);
        commit_panel_glyphs(&mut msdf, glyphs);
    }
    state.text_dirty = false;
}

/// "SENDER -> RECEIVER" for the inspection plate, run through the SAME
/// `socket_label` heuristic the rim pegs render — the card names a wire end
/// exactly the way its peg glyph reads (`RENDER -> TIMIDITY 0`), one visual
/// language instead of a second, more-verbose name for the same socket.
/// Empty when no wires exist (a cleared plate, not a placeholder).
fn describe_selection(snapshot: &PatchGraphSnapshot, selected: usize) -> String {
    let Some(wire) = snapshot.wires.get(selected) else {
        return if snapshot.endpoints.is_empty() {
            "NO ALSA GRAPH".to_string()
        } else {
            "NO WIRES".to_string()
        };
    };
    let name = |addr: (i32, i32)| -> String {
        let Some(ep) = snapshot.endpoints.iter().find(|e| (e.client_id, e.port_id) == addr) else {
            // The endpoint vanished from the snapshot between the wire's poll
            // and this frame — a transient gap, not a client worth labeling.
            return format!("{}:{}", addr.0, addr.1);
        };
        // Same client_id-keyed count `rebuild_patch_scene` feeds its pegs
        // (client_id, not client_name — two clients can share a name), so the
        // card's label matches the peg's glyph exactly, not a near-miss.
        let count = snapshot.endpoints.iter().filter(|e| e.client_id == ep.client_id).count();
        socket_label(&ep.client_name, &ep.port_name, count, ep.port_id)
    };
    format!("{} -> {}", name(wire.src), name(wire.dst))
}

/// Commit glyphs for the per-socket port labels once the font is ready — the
/// same async-font gate as [`fill_patch_text`], but for the primary rim-text
/// layer. Runs just before `fill_patch_text` in the chain, and never clears
/// `text_dirty` (that's `fill_patch_text`'s job), so an early frame with no
/// font yet retries both together on the next tick.
fn fill_port_labels(
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    state: Res<PatchBayState>,
    mut labels: Query<(&PortLabel, &mut MsdfBlockGlyphs)>,
) {
    if !state.text_dirty {
        return;
    }
    let Some(font) = fonts.get(&font_handles.mono) else {
        return;
    };
    let Some(atlas) = atlas.as_deref_mut() else {
        return;
    };
    for (label, mut msdf) in labels.iter_mut() {
        let glyphs = layout_plate_text(&label.0, font, atlas, &mut font_data_map);
        commit_panel_glyphs(&mut msdf, glyphs);
    }
}

// ── Socket label heuristic (display-only; NOT the endpoint registry) ─────────

/// Longest label, in characters, a socket plate shows; longer names truncate.
const LABEL_MAX: usize = 14;

/// Derive the short ALL-CAPS holographic label for a socket peg from its
/// endpoint.
///
/// This is a **display** heuristic — a legibility shortening for the scene,
/// deliberately *not* the symbolic-endpoint registry that
/// `docs/scenes/patchbay.md` open question #2 leaves open (still open: nothing
/// here feeds routing, it only names a floating glyph over a peg).
///
/// - A *meaningful* port name — not "port"/"Port-N"-shaped, not just the
///   client name again — wins, uppercased: `render` → `RENDER`,
///   `capture` → `CAPTURE`.
/// - Otherwise fall back to the shortened client name, plus the port id when
///   the client seats more than one port: TiMidity port 0 → `TIMIDITY 0`;
///   a lone `Midi Through Port-0` → `MIDI THROUGH`.
fn socket_label(client_name: &str, port_name: &str, client_port_count: usize, port_id: i32) -> String {
    let pn = port_name.trim();
    // ALSA often prefixes a port with its client's name ("TiMidity port 0");
    // strip that copy before judging whether what's left carries information.
    let stripped = pn.strip_prefix(client_name).unwrap_or(pn).trim();
    let meaningful = !pn.is_empty()
        && pn != "?"
        && !pn.eq_ignore_ascii_case(client_name)
        && !stripped.is_empty()
        && !is_port_shaped(pn)
        && !is_port_shaped(stripped);
    if meaningful {
        return truncate_chars(&pn.to_uppercase(), LABEL_MAX);
    }

    let client = client_name.to_uppercase();
    if client_port_count > 1 {
        // Keep the disambiguating id even when the client name is long: reserve
        // its width so truncation eats the name, never the number (two ports of
        // one long-named client must not collapse to the same label).
        let id = port_id.to_string();
        let room = LABEL_MAX.saturating_sub(id.len() + 1);
        format!("{} {id}", truncate_chars(&client, room))
    } else {
        truncate_chars(&client, LABEL_MAX)
    }
}

/// True when `s` is an uninformative "port"/"Port-N" name: the literal word
/// `port` (any case) at the start, optionally followed by separators and a
/// number. `portland` and `render` are not port-shaped.
fn is_port_shaped(s: &str) -> bool {
    let lower = s.trim().to_ascii_lowercase();
    let Some(rest) = lower.strip_prefix("port") else {
        return false;
    };
    let rest = rest.trim_start_matches([' ', '-', '_', ':', '#']);
    rest.is_empty() || rest.chars().all(|ch| ch.is_ascii_digit())
}

/// First `max` characters of `s` (char-safe: ALSA names are ASCII, but a stray
/// unicode name must never panic on a byte-boundary slice).
fn truncate_chars(s: &str, max: usize) -> String {
    s.chars().take(max).collect()
}

/// True when a single-port client's group nameplate would just repeat its
/// lone port's label in a different case — e.g. port label `MIDI THROUGH`
/// under nameplate `Midi Through` (live-observed rim-edge collision,
/// `docs/scenes/patchbay.md`). `rebuild_patch_scene` skips spawning the
/// nameplate in that case; the caller scopes the call to
/// `client_port_count == 1` — a multi-port client's nameplate names the
/// WHOLE group, which no single port label speaks for. Exact-length compare
/// (via `eq_ignore_ascii_case`) means a truncated port label never
/// false-positives against a longer, untruncated client name.
fn nameplate_redundant(client_name: &str, port_label: &str) -> bool {
    port_label.eq_ignore_ascii_case(client_name.trim())
}

// ── Live layer + apex helpers ───────────────────────────────────────────────

/// The app's own render port (`kaijutsu-app:render`) — the source of any chord
/// whose traffic the app can observe. Matches `MidiOut::open`'s client/port names.
fn is_render_port(ep: &EndpointInfo) -> bool {
    ep.client_name == RENDER_CLIENT_NAME && ep.port_name == RENDER_PORT_NAME
}

/// The selected chord's apex (its arc midpoint, table space), recomputed from the
/// snapshot with the SAME seat layout + chord path `rebuild_patch_scene` draws —
/// so the inspection card lands ON the wire. `None` when the selection has no
/// drawable chord (no wires, or an endpoint filtered away). Pure; unit-tested.
fn selected_chord_apex(snapshot: &PatchGraphSnapshot, selected: usize) -> Option<Vec3> {
    let wire = snapshot.wires.get(selected)?;

    // Same client grouping as `rebuild_patch_scene` (endpoints are sorted by
    // (client, port), so consecutive runs are exact client groups).
    let mut groups: Vec<(String, usize)> = Vec::new();
    for ep in &snapshot.endpoints {
        match groups.last_mut() {
            Some((name, n)) if *name == ep.client_name => *n += 1,
            _ => groups.push((ep.client_name.clone(), 1)),
        }
    }
    let (seats, _labels) = layout_sockets(&groups);
    let angle_of: Vec<f32> = seats.iter().map(|s| s.angle).collect();
    let by_addr: std::collections::HashMap<(i32, i32), usize> = snapshot
        .endpoints
        .iter()
        .enumerate()
        .map(|(i, e)| ((e.client_id, e.port_id), i))
        .collect();

    let si = *by_addr.get(&wire.src)?;
    let di = *by_addr.get(&wire.dst)?;
    let a1 = *angle_of.get(si)?;
    let a2 = *angle_of.get(di)?;
    let points = chord_points(a1, a2, RIM_R, HOLE_R, CHORD_LIFT, CHORD_SAMPLES);
    Some(Vec3::from_array(points[points.len() / 2]))
}

/// `smoothstep(e0, e1, x)` matching WGSL's — the Hermite ease `chord.wgsl` uses to
/// fade the packet in at the source and out at the dest. The GPU owns the runtime
/// draw; this exists only so [`pulse_band`] can validate that math in a test.
#[cfg(test)]
fn smoothstep_range(e0: f32, e1: f32, x: f32) -> f32 {
    let t = ((x - e0) / (e1 - e0)).clamp(0.0, 1.0);
    t * t * (3.0 - 2.0 * t)
}

/// The traveling packet's brightness at length-position `u` (0 = source, 1 =
/// dest) when the pulse is `age` seconds into its `travel`-second journey — the
/// CPU mirror of `chord.wgsl`'s band math (the shader draws it at runtime; this
/// exists to unit-test the shape). 0 before the pulse and once it has passed
/// (decay to idle); a gaussian crest that rides source→dest as `age` advances.
/// Purely a function of elapsed `age`, so it is frame-rate independent — the
/// packet's position never depends on how many frames it took to get there.
#[cfg(test)]
fn pulse_band(u: f32, age: f32, travel: f32, band_width: f32) -> f32 {
    if age < 0.0 || age > travel {
        return 0.0;
    }
    let progress = age / travel;
    let d = u - progress;
    let packet = (-(d * d) / (band_width * band_width).max(1e-5)).exp();
    let ends = smoothstep_range(0.0, 0.08, progress) * (1.0 - smoothstep_range(0.9, 1.0, progress));
    packet * ends
}

/// Font sizes to try for the inspection plate, largest first (shrink-to-fit).
const INFO_PLATE_SIZES: [f32; 5] = [PLATE_FONT_SIZE, 26.0, 22.0, 18.0, 15.0];

/// The size-choosing predicate `layout_info_text` drives, pulled out so it's
/// testable without a real font. Two passes over `sizes`, largest first:
///   1. any size whose whole text fits ONE unwrapped line (`unwrapped_width`,
///      probed with no wrap constraint at all). Requiring only the widest
///      *word* to fit (the old, single-pass behavior) let a short string like
///      `RENDER -> TIMIDITY 0` land on a size just barely too big for the
///      plate — the trailing "0" wrapped onto its own line even though the
///      whole string would have fit one step down (`docs/issues.md`).
///   2. only if no size clears pass 1, the old wrap-allowed fit
///      (`wrapped_metrics`: `(content_widths.min, height)`) — a genuinely
///      long multi-hop wire name still wraps rather than truncating.
/// Falls back to the smallest size (the floor) if nothing fits either pass.
fn choose_info_plate_size(
    sizes: &[f32],
    max_w: f32,
    max_h: f32,
    mut unwrapped_width: impl FnMut(f32) -> f32,
    mut wrapped_metrics: impl FnMut(f32) -> (f32, f32),
) -> f32 {
    if let Some(&size) = sizes.iter().find(|&&size| unwrapped_width(size) <= max_w) {
        return size;
    }
    if let Some(&size) = sizes.iter().find(|&&size| {
        let (content_min, height) = wrapped_metrics(size);
        content_min <= max_w && height <= max_h
    }) {
        return size;
    }
    sizes.last().copied().unwrap_or(0.0)
}

/// Shrink-to-fit MSDF layout for the inspection plate (`PLATE_TEX_W`×`PLATE_TEX_H`):
/// steps the font size down via [`choose_info_plate_size`] until the shaped
/// `client:port -> client:port` fits the plate's usable box, then collects
/// glyphs at that size. Same machinery as `room::layout_plate_text` (brush,
/// atlas, MSDF collect) — it only adds the fit loop the fixed-size helper
/// can't do, so a long wire name no longer overflows the frame
/// (`docs/issues.md`).
fn layout_info_text(
    text: &str,
    font: &VelloFont,
    atlas: &mut MsdfAtlas,
    font_data_map: &mut FontDataMap,
) -> Vec<PositionedGlyph> {
    if text.is_empty() {
        return Vec::new();
    }
    let brush = bevy_color_to_brush(Color::srgba(0.82, 0.88, 0.97, 0.9));
    let max_w = PLATE_TEX_W - 2.0 * PLATE_PAD;
    let max_h = PLATE_TEX_H - 2.0 * PLATE_PAD;
    let style = |size: f32| VelloTextStyle { font_size: size, line_height: 1.1, ..default() };

    let chosen = choose_info_plate_size(
        &INFO_PLATE_SIZES,
        max_w,
        max_h,
        // No wrap constraint at all: `.width()` is the text's true one-line
        // width, so this only accepts a size that needs no wrapping.
        |size| font.layout(text, &style(size), VelloTextAlign::Middle, None).width(),
        |size| {
            let probe = font.layout(text, &style(size), VelloTextAlign::Middle, Some(max_w));
            (probe.calculate_content_widths().min, probe.height())
        },
    );

    let layout = font.layout(text, &style(chosen), VelloTextAlign::Middle, Some(max_w));
    for line in layout.lines() {
        for item in line.items() {
            if let parley::PositionedLayoutItem::GlyphRun(gr) = item {
                font_data_map.register(gr.run().font());
            }
        }
    }
    collect_msdf_glyphs(&layout, &[], &brush, (PLATE_PAD as f64, PLATE_PAD as f64), atlas)
}

// ── Plate + ribbon helpers ──────────────────────────────────────────────────

/// The shared rotation every wheel text plate bakes at spawn (or re-pose,
/// [`position_info_plate`]) instead of the old per-position camera billboard
/// (`face_camera`, deleted 2026-07-10 — the live-found regression: a rotation
/// baked to face `PB_CAM_POS` in the scene's PRE-mount local frame reads
/// sideways/upside-down once [`STATION_W_PLACEMENT`] pitches and yaws that
/// frame onto the wall, because local "up" no longer survives the mount).
/// Constant, not position-dependent: the wheel is one flat mounted plane, so
/// every plate on it shares a single facing — unlike the old horizontal
/// table, where a fixed off-axis camera needed a per-position billboard to
/// square each label toward the eye.
///
/// A `Rectangle` mesh lies flat in the local XY-plane, its readable face
/// toward local +Z and its texture-up along local +Y (the same convention
/// `face_camera` used to name). This sends that face to the wheel's own local
/// +Y — which [`placement_rotation`] carries to world +X, out of the wall and
/// into the room, the SAME target [`STATION_W_PLACEMENT`]'s doc derives for
/// the table's own normal — and the texture-up to the wheel's local +Z, which
/// placement carries to world +Y, screen-up. Locked ±1e-5 by the axis tests
/// below (trust the tests over this prose if the two ever disagree).
fn upright_wall_facing() -> Quat {
    Quat::from_rotation_x(std::f32::consts::FRAC_PI_2) * Quat::from_rotation_y(std::f32::consts::PI)
}

/// A plate transform at `pos` using the shared [`upright_wall_facing`]
/// rotation — the direct replacement for the old `face_camera(pos)` at every
/// text-bearing site on the wheel (port labels, group nameplates, title,
/// legend, the inspection card). Position is untouched by the wall-mount text
/// fix; only the in-plane rotation changed.
fn upright_plate(pos: Vec3) -> Transform {
    Transform::from_translation(pos).with_rotation(upright_wall_facing())
}

/// A floating MSDF text plate with the shared upright wall-facing (borderless,
/// label-style; text committed later by [`fill_patch_text`]). Spawned
/// `Visibility::Hidden`: every caller (title/legend/info) is the dive LOD, shown
/// only while dived by `apply_patch_lod`, so the room-scale ambient stays
/// text-free.
fn plate_bundle(
    meshes: &mut Assets<Mesh>,
    card_materials: &mut Assets<WellCardMaterial>,
    images: &mut Assets<Image>,
    pos: Vec3,
    scale: f32,
) -> impl Bundle {
    let mesh = meshes.add(Rectangle::new(210.0 * scale, 62.0 * scale));
    let (image, panel) = create_msdf_panel(
        images,
        crate::view::room::PLATE_TEX_W as u32,
        crate::view::room::PLATE_TEX_H as u32,
    );
    let material = card_materials.add(WellCardMaterial {
        texture: image,
        accent: Vec4::ZERO,
        params: Vec4::ZERO,
        shape: Vec4::new(
            crate::view::room::PLATE_TEX_W / crate::view::room::PLATE_TEX_H,
            0.0,
            0.0,
            0.0,
        ),
        border: Vec4::ZERO,
        dim: Vec4::new(0.85, 0.0, 0.0, 0.0),
    });
    (
        Mesh3d(mesh),
        MeshMaterial3d(material),
        upright_plate(pos),
        Visibility::Hidden,
        panel,
    )
}

// ── Ribbon mesh ─────────────────────────────────────────────────────────────

/// A double-sided flat ribbon along `points`, `width` across — the chord's body.
/// UV.x runs 0→1 along the length (source→dest); `chord.wgsl` rides the traffic
/// packet down it. Built double-sided (both windings) because `ChordMaterial`
/// sets no `cull_mode` and the fixed camera can catch either face of a bowing arc.
fn ribbon_mesh(points: &[[f32; 3]], width: f32) -> Mesh {
    use bevy::asset::RenderAssetUsages;
    use bevy::mesh::{Indices, PrimitiveTopology};

    let n = points.len().max(2);
    let mut positions: Vec<[f32; 3]> = Vec::with_capacity(n * 2);
    let mut normals: Vec<[f32; 3]> = Vec::with_capacity(n * 2);
    let mut uvs: Vec<[f32; 2]> = Vec::with_capacity(n * 2);
    let half = width / 2.0;

    for i in 0..points.len() {
        let p = Vec3::from_array(points[i]);
        let prev = Vec3::from_array(points[i.saturating_sub(1)]);
        let next = Vec3::from_array(points[(i + 1).min(points.len() - 1)]);
        let dir = (next - prev).normalize_or(Vec3::X);
        let side = Vec3::Y.cross(dir).normalize_or(Vec3::Z) * half;
        let t = i as f32 / (points.len() - 1).max(1) as f32;
        positions.push((p - side).to_array());
        positions.push((p + side).to_array());
        normals.push([0.0, 1.0, 0.0]);
        normals.push([0.0, 1.0, 0.0]);
        uvs.push([t, 0.0]);
        uvs.push([t, 1.0]);
    }

    let mut indices: Vec<u32> = Vec::with_capacity((points.len() - 1) * 12);
    for i in 0..(points.len() as u32 - 1) {
        let a = i * 2;
        // Front face, then the same two triangles wound the other way so the
        // ribbon renders from below too (the unlit material ignores normals).
        indices.extend_from_slice(&[a, a + 1, a + 2, a + 2, a + 1, a + 3]);
        indices.extend_from_slice(&[a + 2, a + 1, a, a + 3, a + 1, a + 2]);
    }

    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions)
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_UV_0, uvs)
        .with_inserted_indices(Indices::U32(indices))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use crate::patch_graph::{EndpointInfo, WireInfo};

    use super::*;

    fn non_empty_snapshot() -> PatchGraphSnapshot {
        PatchGraphSnapshot {
            endpoints: vec![EndpointInfo {
                client_id: 128,
                port_id: 0,
                client_name: "TiMidity".into(),
                port_name: "port 0".into(),
                is_source: true,
                is_sink: false,
            }],
            wires: vec![WireInfo { src: (14, 0), dst: (128, 0) }],
        }
    }

    /// The shape `PatchBayState` is left in after a room session: the resource
    /// (and its snapshot) outlive the entities `RoomRoot` carried, but nothing
    /// has re-armed the dirty flags for the next room entry yet.
    fn persisted_after_exit() -> PatchBayState {
        PatchBayState {
            snapshot: non_empty_snapshot(),
            selected: 0,
            scene_dirty: false,
            text_dirty: false,
            timer: Timer::from_seconds(POLL_SECS, TimerMode::Repeating),
        }
    }

    // -- arm_scene (armed from room::enter_room) -----------------------

    #[test]
    fn arm_scene_forces_both_dirty_flags_true() {
        let mut state = persisted_after_exit();
        arm_scene(&mut state);
        assert!(
            state.scene_dirty,
            "room re-entry must force a rebuild even when the graph hasn't changed since the last visit"
        );
        assert!(state.text_dirty);
    }

    #[test]
    fn arm_scene_primes_the_timer_to_fire_on_the_next_tick() {
        let mut state = persisted_after_exit();
        arm_scene(&mut state);
        assert!(!state.timer.just_finished(), "not finished until it's ticked");
        assert!(state.timer.tick(Duration::from_millis(1)).just_finished());
    }

    #[test]
    fn arm_scene_leaves_the_persisted_snapshot_untouched() {
        let mut state = persisted_after_exit();
        let before = state.snapshot.clone();
        arm_scene(&mut state);
        assert_eq!(
            state.snapshot, before,
            "rebuild_patch_scene reads the persisted snapshot; arm_scene must not touch it"
        );
    }

    // -- placement seam (the room-furniture transform) -----------------

    #[test]
    fn the_placement_seam_lands_the_patch_bay_origin_at_its_room_station() {
        // The scene's local origin maps to exactly the station translation — the
        // one knob that puts the whole circle at W.
        let at = placement_to_room(&STATION_W_PLACEMENT, Vec3::ZERO);
        assert_eq!(at, STATION_W_PLACEMENT.translation);
    }

    #[test]
    fn the_placement_seam_sends_a_local_height_into_a_world_east_offset() {
        // A point straight up in patch-bay-local space (the table's own
        // normal) no longer rises: the wall-mount pitch+yaw sends local +Y
        // into world +X — into the room — scaled by `scale`, and leaves Y/Z
        // untouched. The rotated seam's version of the old
        // "scale-alone" invariant.
        let p = &STATION_W_PLACEMENT;
        let out = placement_to_room(p, Vec3::new(0.0, 100.0, 0.0));
        assert!((out.x - (p.translation.x + 100.0 * p.scale)).abs() < 1e-3, "{out:?}");
        assert!((out.y - p.translation.y).abs() < 1e-3, "{out:?}");
        assert!((out.z - p.translation.z).abs() < 1e-3, "{out:?}");
    }

    // -- rotation composition (locks `STATION_W_PLACEMENT`'s derivation) --

    /// W's pitch/yaw with the translation/scale zeroed out — isolates the
    /// pure rotation `STATION_W_PLACEMENT`'s doc derives, the same way its
    /// comment works the math.
    fn w_rotation_only() -> StationPlacement {
        StationPlacement {
            translation: Vec3::ZERO,
            scale: 1.0,
            pitch: STATION_W_PLACEMENT.pitch,
            yaw: STATION_W_PLACEMENT.yaw,
        }
    }

    #[test]
    fn placement_rotation_sends_local_up_into_the_room_along_world_east() {
        // The rotation derivation's first target, locked: local +Y (the
        // table's own normal) must land on world +X — off the W wall, into
        // the room.
        let mapped = placement_to_room(&w_rotation_only(), Vec3::Y);
        assert!((mapped - Vec3::X).length() < 1e-5, "{mapped:?}");
    }

    #[test]
    fn placement_rotation_sends_the_old_camera_edge_to_the_top_of_the_mounted_face() {
        // The roll tie-breaker, locked: local +Z (the standalone scene's
        // camera-side edge) must land on world +Y — the TOP of the mounted
        // face — never world −Y (which would hang the wheel upside down).
        let mapped = placement_to_room(&w_rotation_only(), Vec3::Z);
        assert!((mapped - Vec3::Y).length() < 1e-5, "{mapped:?}");
    }

    // -- upright_wall_facing (the shared text-orientation recipe) -------
    //
    // The live-found regression this fixes: every wheel text plate baked its
    // facing against the scene's PRE-mount local frame (`face_camera`,
    // deleted), so mounting the wheel on the wall left every label sideways,
    // diagonal, or upside down. These tests carry the plate rotation THROUGH
    // `placement_rotation(&STATION_W_PLACEMENT)`, the same way a real plate's
    // world orientation is built (root's own transform is identity, so a
    // plate's world rotation is exactly `placement_rotation * plate_rotation`).

    #[test]
    fn upright_wall_facing_sends_the_plate_normal_toward_the_room() {
        // The mesh's readable face (local +Z, `face_camera`'s old convention)
        // must land on world +X once the mount carries it off the wall — the
        // SAME target `STATION_W_PLACEMENT`'s own doc derives for the table's
        // normal, so a plate reads square-on to a viewer standing in the room.
        let world_normal = placement_rotation(&STATION_W_PLACEMENT) * (upright_wall_facing() * Vec3::Z);
        assert!((world_normal - Vec3::X).length() < 1e-5, "{world_normal:?}");
    }

    #[test]
    fn upright_wall_facing_sends_the_text_up_axis_to_world_up() {
        // The mesh's texture-up (local +Y) must land on world +Y — screen-
        // upright text, the whole point of the fix.
        let world_up = placement_rotation(&STATION_W_PLACEMENT) * (upright_wall_facing() * Vec3::Y);
        assert!((world_up - Vec3::Y).length() < 1e-5, "{world_up:?}");
    }

    #[test]
    fn upright_wall_facing_sends_the_plate_width_axis_into_the_wheels_own_horizontal() {
        // The mesh's own width axis (local +X, the direction text reads
        // across) lands back on the wheel's own local X (sign aside — a
        // label's width is symmetric about its center) — the SAME local axis
        // every rim socket's `RIM_R * cos(angle)` position is built from.
        // `PORT_LABEL_R`'s doc leans on this: a label's width always runs
        // along this one fixed axis, so at the two seats where it happens to
        // be the RADIUS rather than the rim's tangent, the label overhangs
        // toward the center instead of sliding along the rim.
        let mapped = upright_wall_facing() * Vec3::X;
        assert!((mapped.abs() - Vec3::X).length() < 1e-5, "{mapped:?}");
    }

    // The old dive-camera-pose test (dive_camera_pose, PB_CAM_POS/LOOK, all
    // deleted this slice) moved to `room::mod`'s `fullscreen_pose_*` tests —
    // the camera pose is computed there now, decoupled from this placement.

    // -- PatchBayAlsa::clear_failed_open --------------------------------

    #[test]
    fn clear_failed_open_resets_a_latched_failure_to_unopened() {
        let mut alsa = PatchBayAlsa { reader: Some(None) };
        alsa.clear_failed_open();
        assert!(
            alsa.reader.is_none(),
            "must go back to None so poll_patch_graph's get_or_insert_with retries the open"
        );
    }

    #[test]
    fn clear_failed_open_is_a_no_op_when_never_opened() {
        let mut alsa = PatchBayAlsa { reader: None };
        alsa.clear_failed_open();
        assert!(alsa.reader.is_none());
    }

    // The healthy `Some(Some(_))` arm (left alone by `clear_failed_open`) needs
    // a real `PatchGraphReader`, which needs a live ALSA sequencer — it's
    // exercised by the `#[ignore]`d `alsa_smoke` path in `patch_graph.rs`, not
    // here.

    // -- socket_label (display heuristic — patchbay.md open question #2 stays open) --

    #[test]
    fn a_meaningful_port_name_becomes_its_own_uppercase_label() {
        // The port name carries the information; the client name is redundant.
        assert_eq!(socket_label("kaijutsu-app", "render", 1, 0), "RENDER");
        assert_eq!(socket_label("kaijutsu-app", "capture", 2, 1), "CAPTURE");
    }

    #[test]
    fn a_port_shaped_name_falls_back_to_client_plus_id_on_a_multiport_client() {
        // TiMidity seats four "port N" ports: the name says nothing, so the id
        // has to disambiguate. Both the bare "port 0" and the ALSA-prefixed
        // "TiMidity port 3" resolve the same way.
        assert_eq!(socket_label("TiMidity", "port 0", 4, 0), "TIMIDITY 0");
        assert_eq!(socket_label("TiMidity", "TiMidity port 3", 4, 3), "TIMIDITY 3");
    }

    #[test]
    fn a_client_prefixed_port_on_a_single_port_client_drops_the_redundant_id() {
        // A lone "Midi Through Port-0": the "Port-0" is noise and there is only
        // one port, so the client name alone reads cleanest.
        assert_eq!(socket_label("Midi Through", "Midi Through Port-0", 1, 0), "MIDI THROUGH");
    }

    #[test]
    fn a_port_named_after_its_own_client_falls_back_to_the_client() {
        assert_eq!(socket_label("FLUID Synth", "FLUID Synth", 1, 0), "FLUID SYNTH");
    }

    #[test]
    fn a_long_meaningful_label_is_truncated_to_fit_the_plate() {
        let label = socket_label("app", "a-very-long-descriptive-port", 1, 0);
        assert!(label.chars().count() <= LABEL_MAX, "{label:?} exceeds {LABEL_MAX}");
        assert!(label.starts_with("A-VERY"), "{label:?}");
    }

    #[test]
    fn a_long_client_fallback_sacrifices_the_name_but_keeps_the_id() {
        // Truncation eats the name, never the number — otherwise two ports of
        // one long-named client would collapse to the same label.
        let label = socket_label("a-very-long-synth-name", "port 2", 3, 2);
        assert!(label.chars().count() <= LABEL_MAX, "{label:?} exceeds {LABEL_MAX}");
        assert!(label.ends_with(" 2"), "{label:?} lost its id");
    }

    #[test]
    fn is_port_shaped_matches_only_the_uninformative_port_names() {
        assert!(is_port_shaped("port"));
        assert!(is_port_shaped("port 0"));
        assert!(is_port_shaped("Port-12"));
        assert!(is_port_shaped("PORT_3"));
        assert!(!is_port_shaped("render"));
        assert!(!is_port_shaped("portland"));
    }

    // -- nameplate_redundant (single-port nameplate suppression) --------

    #[test]
    fn nameplate_redundant_true_for_the_live_observed_collision() {
        // The exact reported case: port label "MIDI THROUGH" floating right
        // over a nameplate that just spells the same client name lowercase.
        assert!(nameplate_redundant("Midi Through", "MIDI THROUGH"));
    }

    #[test]
    fn nameplate_redundant_false_when_the_port_label_names_something_else() {
        // A meaningful port name (RENDER) says something the client name
        // (kaijutsu-app) doesn't — the nameplate still earns its keep.
        assert!(!nameplate_redundant("kaijutsu-app", "RENDER"));
    }

    #[test]
    fn nameplate_redundant_false_when_truncation_breaks_the_match() {
        // socket_label truncates to LABEL_MAX; a truncated port label must
        // never false-positive against the longer, untruncated client name
        // eq_ignore_ascii_case would otherwise reject on length alone, but
        // spell it out so the invariant stays visible here too.
        let client = "a-very-long-synth-name-that-is-long";
        let truncated = truncate_chars(&client.to_uppercase(), LABEL_MAX);
        assert!(!nameplate_redundant(client, &truncated));
    }

    // -- describe_selection (uses socket_label — same language as the pegs) --

    fn render_to_multi_port_timidity_snapshot() -> PatchGraphSnapshot {
        // TiMidity seats two ports here (unlike `render_to_synth_snapshot`'s
        // single port), so its end exercises socket_label's port-shaped
        // multi-port fallback instead of the single-port case.
        PatchGraphSnapshot {
            endpoints: vec![
                EndpointInfo {
                    client_id: 128,
                    port_id: 0,
                    client_name: "TiMidity".into(),
                    port_name: "port 0".into(),
                    is_source: false,
                    is_sink: true,
                },
                EndpointInfo {
                    client_id: 128,
                    port_id: 1,
                    client_name: "TiMidity".into(),
                    port_name: "port 1".into(),
                    is_source: false,
                    is_sink: true,
                },
                EndpointInfo {
                    client_id: 129,
                    port_id: 0,
                    client_name: "kaijutsu-app".into(),
                    port_name: "render".into(),
                    is_source: true,
                    is_sink: false,
                },
            ],
            wires: vec![WireInfo { src: (129, 0), dst: (128, 0) }],
        }
    }

    #[test]
    fn describe_selection_speaks_the_same_language_as_the_socket_pegs() {
        // A meaningful port name on one end (RENDER) and a port-shaped
        // multi-port fallback on the other (TIMIDITY 0) — the exact string a
        // pair of socket_label-driven pegs would show, not a second,
        // more-verbose name for the same wire.
        let text = describe_selection(&render_to_multi_port_timidity_snapshot(), 0);
        assert_eq!(text, "RENDER -> TIMIDITY 0");
    }

    #[test]
    fn describe_selection_falls_back_to_raw_ids_for_a_vanished_endpoint() {
        // `wire.src` in `non_empty_snapshot` names client_id 14, which isn't
        // in `endpoints` — a transient gap between the ALSA event and the
        // next poll's snapshot, not a client to invent a label for.
        let text = describe_selection(&non_empty_snapshot(), 0);
        assert_eq!(text, "14:0 -> TIMIDITY");
    }

    // -- is_render_port -------------------------------------------------

    fn ep(client_id: i32, port_id: i32, client_name: &str, port_name: &str) -> EndpointInfo {
        EndpointInfo {
            client_id,
            port_id,
            client_name: client_name.into(),
            port_name: port_name.into(),
            is_source: true,
            is_sink: false,
        }
    }

    #[test]
    fn is_render_port_matches_only_our_own_render_endpoint() {
        assert!(is_render_port(&ep(129, 0, "kaijutsu-app", "render")));
        // A synth's port, our ear, and a mis-named app port are not the send seam.
        assert!(!is_render_port(&ep(128, 0, "TiMidity", "port 0")));
        assert!(!is_render_port(&ep(200, 0, "kaijutsu-ear", "capture")));
        assert!(!is_render_port(&ep(129, 1, "kaijutsu-app", "in")));
    }

    // -- pulse_band: the traveling-packet math --------------------------

    /// Argmax of the packet across the chord length (11 samples) — where the
    /// crest sits, in length-UV.
    fn crest_u(age: f32) -> f32 {
        (0..=10)
            .map(|i| i as f32 / 10.0)
            .max_by(|&a, &b| {
                pulse_band(a, age, PULSE_TRAVEL_SECS, PULSE_BAND_WIDTH)
                    .total_cmp(&pulse_band(b, age, PULSE_TRAVEL_SECS, PULSE_BAND_WIDTH))
            })
            .unwrap()
    }

    #[test]
    fn pulse_band_is_dark_before_the_send_and_after_the_packet_passes() {
        // Before the pulse (negative age) and past the travel time, every point on
        // the chord has decayed to idle — no packet.
        for &u in &[0.0, 0.25, 0.5, 0.75, 1.0] {
            assert_eq!(pulse_band(u, -0.01, PULSE_TRAVEL_SECS, PULSE_BAND_WIDTH), 0.0);
            assert_eq!(
                pulse_band(u, PULSE_TRAVEL_SECS + 0.01, PULSE_TRAVEL_SECS, PULSE_BAND_WIDTH),
                0.0,
            );
        }
        // The resting sentinel (a send far in the past ⇒ a huge age) is idle too.
        let idle_age = 10.0 - PULSE_IDLE;
        assert_eq!(pulse_band(0.5, idle_age, PULSE_TRAVEL_SECS, PULSE_BAND_WIDTH), 0.0);
    }

    #[test]
    fn pulse_band_crest_tracks_elapsed_age_source_to_dest() {
        // The crest sits at u = age/travel: its position is set purely by elapsed
        // wall-clock age, never by frame count, so the pulse looks the same at any
        // frame rate (the reason the band rides an absolute timestamp, not an
        // accumulator). Crest resolution is one 0.1 sample.
        for frac in [0.15_f32, 0.4, 0.6, 0.85] {
            let u = crest_u(frac * PULSE_TRAVEL_SECS);
            assert!((u - frac).abs() <= 0.11, "age frac {frac}: crest at u={u}");
        }
    }

    // -- selected_chord_apex --------------------------------------------

    fn render_to_synth_snapshot() -> PatchGraphSnapshot {
        // Endpoints sorted by (client, port) as `observe`/`snapshot` deliver them.
        PatchGraphSnapshot {
            endpoints: vec![
                EndpointInfo {
                    client_id: 128,
                    port_id: 0,
                    client_name: "TiMidity".into(),
                    port_name: "port 0".into(),
                    is_source: false,
                    is_sink: true,
                },
                EndpointInfo {
                    client_id: 129,
                    port_id: 0,
                    client_name: "kaijutsu-app".into(),
                    port_name: "render".into(),
                    is_source: true,
                    is_sink: false,
                },
            ],
            wires: vec![WireInfo { src: (129, 0), dst: (128, 0) }],
        }
    }

    #[test]
    fn selected_chord_apex_lands_on_the_arc_between_the_rim_and_the_hole() {
        let apex = selected_chord_apex(&render_to_synth_snapshot(), 0).expect("a drawable chord");
        let r = (apex.x * apex.x + apex.z * apex.z).sqrt();
        assert!(r > HOLE_R && r < RIM_R, "apex bows in the open-center corridor, r={r}");
        // The apex is the arc's high point — near the chord lift, above the table.
        assert!(apex.y > CHORD_LIFT * 0.9, "apex near the lift peak, y={}", apex.y);
    }

    #[test]
    fn selected_chord_apex_is_none_without_a_drawable_chord() {
        // No wires at all ⇒ the card falls back to the edge pose.
        assert!(selected_chord_apex(&PatchGraphSnapshot::default(), 0).is_none());
        // A selection past the end of the wire list ⇒ likewise.
        assert!(selected_chord_apex(&render_to_synth_snapshot(), 9).is_none());
    }

    // -- choose_info_plate_size (the wrap-avoidance fix) -----------------
    //
    // `layout_info_text` needs a real shaped font to probe widths, so these
    // drive the extracted picker with synthetic per-size metrics instead —
    // no font fixture required.

    #[test]
    fn choose_info_plate_size_picks_the_largest_size_that_fits_one_line_unwrapped() {
        let sizes = [30.0_f32, 20.0, 10.0];
        // Every size fits unwrapped; the largest should win, and the
        // wrap-allowed fallback must never even be consulted.
        let chosen = choose_info_plate_size(
            &sizes,
            100.0,
            50.0,
            |_size| 90.0,
            |_size| panic!("pass 1 already found a fit; pass 2 must not run"),
        );
        assert_eq!(chosen, 30.0);
    }

    #[test]
    fn choose_info_plate_size_rejects_a_size_that_only_fits_after_wrapping() {
        // The reported bug, reproduced directly: "RENDER -> TIMIDITY 0" at
        // the largest size only fits the plate if a word is allowed to wrap
        // onto its own line — that's the OLD algorithm's `content_widths.min`
        // check passing at 30.0. The new picker must skip 30.0 (its unwrapped
        // width overflows the box) and land on 20.0, which fits unwrapped.
        let sizes = [30.0_f32, 20.0];
        let chosen = choose_info_plate_size(
            &sizes,
            100.0,
            50.0,
            |size| if size == 30.0 { 120.0 } else { 90.0 },
            // Old wrap-allowed check would happily accept 30.0 here (a lone
            // word is narrower than the whole string) — proof the fix no
            // longer lets this pass win.
            |_size| (40.0, 45.0),
        );
        assert_eq!(chosen, 20.0, "must prefer the unwrapped fit over a wrap-allowed one");
    }

    #[test]
    fn choose_info_plate_size_falls_back_to_wrap_allowed_when_nothing_fits_unwrapped() {
        // A genuinely long multi-hop wire name: no size fits on one line, so
        // the picker must fall back to the old wrap-allowed fit rather than
        // flooring straight to the smallest size.
        let sizes = [30.0_f32, 20.0, 10.0];
        let chosen = choose_info_plate_size(
            &sizes,
            50.0,
            40.0,
            |_size| 200.0, // never fits unwrapped at any size
            |size| if size == 30.0 { (60.0, 999.0) } else { (40.0, 30.0) },
        );
        assert_eq!(chosen, 20.0);
    }

    #[test]
    fn choose_info_plate_size_floors_at_the_smallest_size_when_nothing_fits_either_pass() {
        let sizes = [30.0_f32, 20.0, 10.0];
        let chosen = choose_info_plate_size(&sizes, 10.0, 10.0, |_size| 999.0, |_size| (999.0, 999.0));
        assert_eq!(chosen, 10.0, "the smallest size is the floor, even unfit");
    }

    // -- zoom gate (2026-07-10 evening, the fullscreen-panel pivot) -----
    //
    // The old "shell lifecycle: leaving from a dive" tests lived here because
    // `Screen::PatchBay` had its own `OnExit` that could be bypassed —
    // `exit_patch_bay`, deleted along with the state itself. That whole class
    // of test moved to `room::mod`'s `exit_room_always_tears_down_and_clears_any_lingering_zoom`:
    // there is only one screen (`Screen::Room`) left for this scene graph to
    // occupy, so there is only one exit left to test. What's left here is the
    // gate itself.

    #[test]
    fn is_zoomed_into_matches_only_the_given_station() {
        assert!(is_zoomed_into(Some(Station::PatchBay), Station::PatchBay));
        assert!(!is_zoomed_into(Some(Station::Tracks), Station::PatchBay));
        assert!(!is_zoomed_into(None, Station::PatchBay));
    }
}
