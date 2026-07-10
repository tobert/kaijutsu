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
//! ONE Amy-tunable placement transform ([`STATION_W_PLACEMENT`]) that seats it
//! on the room-built W dais at station scale — internal coordinates are
//! untouched. It is spawned when the room spawns ([`spawn_furniture`], called
//! from `room::enter_room`) and lives as long as the room; diving is a
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
use crate::ui::screen::{Screen, in_shell};
use crate::view::palette;
use crate::view::room::nav::{Station, StationCarousel};
use crate::view::room::{
    PLATE_FONT_SIZE, PLATE_PAD, PLATE_TEX_H, PLATE_TEX_W, RoomCamera, RoomRoot, RoomState,
    layout_plate_text, teardown_room,
};
use crate::view::time_well::panel::{commit_panel_glyphs, create_msdf_panel};
use geometry::{chord_points, layout_sockets};

// ── Station placement seam (Amy-tunable — the ONE knob) ──────────────────────
// Where the patch-bay wheel stands as the west station itself. This is the
// single seam the slice-B decision (`docs/scenes/shell.md`, open question 3,
// DECIDED 2026-07-09) rides on: the whole `PatchBayRoot` subtree is a child of
// a placement entity carrying this transform, so all positioning/scaling
// happens HERE and the patch bay's internal coordinates never change. The
// room builds a dais at the W bearing sized to the SAME `palette::STATION_W_*`
// numbers this placement reads (`palette.rs`'s "Station W contract" — room
// and patch bay agree there, never by eyeballing each other). The dive camera
// is the local `PB_CAM_POS/LOOK` mapped through this same transform
// (`dive_camera_pose`), so the dived view frames the table exactly as the
// standalone scene did — one seam: edit this constant (or the shared contract
// it reads) and everything, including the dive camera, follows via the
// similarity transform.

/// A rigid-plus-uniform-scale placement of a scene into room space.
struct StationPlacement {
    /// Room-space translation of the scene's local origin.
    translation: Vec3,
    /// Uniform scale applied to the scene's local coordinates.
    scale: f32,
    /// Yaw about +Y (radians). `FRAC_PI_2` turns the scene's local +Z (the
    /// standalone camera's side) toward world +X — the same side the room's
    /// W-focus camera studies the table from — so the dive is a lean-in, not a
    /// jump to the far side.
    yaw: f32,
}

/// The patch bay's placement at W. Amy, 2026-07-10: the wheel IS the west
/// station now — no pylon, no separate nameplate (this supersedes the earlier
/// "pending floor-placement alternative" note the comment here used to carry;
/// the alternative was decided). The room builds a dais at `palette::STATION_W_X`
/// / `_DAIS_TOP_Y` / `_DAIS_R`; this seats the wheel's local origin (its
/// tabletop plane) flush on the dais top, at `palette::STATION_W_SCALE`. Scale
/// math: `TABLE_OUTER_R` (348 local units) × 0.34 ≈ 118 world — a peer to the
/// well table's 120, not a miniature.
const STATION_W_PLACEMENT: StationPlacement = StationPlacement {
    translation: Vec3::new(palette::STATION_W_X, palette::STATION_W_DAIS_TOP_Y, 0.0),
    scale: palette::STATION_W_SCALE,
    yaw: std::f32::consts::FRAC_PI_2,
};

// ── Palette helpers (this scene's own copy — `room::lin`/`lin_scaled` are
// private to that module, so the scene-family material discipline gets a
// second small implementation here rather than a cross-module reach) ────────

/// A linear-rgb [`Color`] from an `[f32; 3]` palette value.
fn lin(c: [f32; 3]) -> Color {
    Color::LinearRgba(LinearRgba::rgb(c[0], c[1], c[2]))
}

/// [`lin`] scaled by a brightness tier — the palette's hue × LDR/HDR-tier
/// convention (`palette.rs` header).
fn lin_scaled(c: [f32; 3], k: f32) -> Color {
    Color::LinearRgba(LinearRgba::rgb(c[0] * k, c[1] * k, c[2] * k))
}

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

/// Camera pose (patch-bay LOCAL space): tilted look down onto the table. Under
/// the standalone scene this was the room-space pose; now it is mapped through
/// [`STATION_W_PLACEMENT`] to place the dive camera in room space
/// ([`dive_camera_pose`]) so the dived framing is identical to the old scene.
const PB_CAM_POS: Vec3 = Vec3::new(0.0, 470.0, 590.0);
const PB_CAM_LOOK: Vec3 = Vec3::new(0.0, 0.0, -40.0);

/// Wire hue — crimson = MIDI fabric (`docs/scenes/patchbay.md`, wire grammar).
/// Already HDR (>1.0) so a live wire blooms; the selected chord multiplies it.
const WIRE_HUE: LinearRgba = LinearRgba::rgb(1.4, 0.16, 0.24);
/// Idle-glow multiplier for the inspected chord (a brighter, bloomier wire).
const CHORD_SELECTED_GAIN: f32 = 3.4;

// ── Live layer (Amy-tunable) ────────────────────────────────────────────────
// Traffic pulses ride the chord src→dest when the render port sends MIDI. The
// packet is animated on the GPU against `globals.time` (one uniform write per
// pulse, `chord.wgsl`); `pulse_band` below is the CPU mirror of that math.

/// Seconds a traffic packet takes to travel a chord source→dest.
const PULSE_TRAVEL_SECS: f32 = 0.42;
/// Gaussian half-width of the packet in length-UV (0..1 across the chord).
const PULSE_BAND_WIDTH: f32 = 0.16;
/// Peak brightness added at the packet crest (HDR → bloom = "live action").
const PULSE_GAIN: f32 = 6.0;
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
const PORT_LABEL_R: f32 = RIM_R * 1.02;
/// Amy-tunable: raised from 50.0 — a rim-edge seat (tangent nearly radial to
/// `PB_CAM_POS`) let this collide with the group nameplate below it. The two
/// text tiers must not overlap in projection from the fixed camera at any
/// seat angle; nudge this (and `GROUP_PLATE_Y`/`GROUP_PLATE_R` below) if they
/// still do.
const PORT_LABEL_Y: f32 = 64.0;
const PORT_LABEL_W: f32 = 108.0;
/// Height keeps the shared plate texture's aspect so the glyphs don't stretch.
const PORT_LABEL_H: f32 =
    PORT_LABEL_W * crate::view::room::PLATE_TEX_H / crate::view::room::PLATE_TEX_W;
const PORT_LABEL_DIM: f32 = 1.0;
/// Client group nameplates — the SUPPORTING layer: dimmer and further out than
/// the port labels, so the two text tiers read as hierarchy, not noise.
/// Amy-tunable: pushed further out (1.22 → 1.32) and lower (34.0 → 16.0) for
/// the same reason as `PORT_LABEL_Y` above — the two text tiers must not
/// overlap in projection from the fixed camera at any seat angle.
const GROUP_PLATE_R: f32 = RIM_R * 1.32;
const GROUP_PLATE_Y: f32 = 16.0;
const GROUP_PLATE_DIM: f32 = 0.5;

// ── Placement math (pure; the room-furniture seam) ───────────────────────────

/// The `Transform` for the placement entity that re-roots the whole patch-bay
/// subtree into room space — translation, yaw about +Y, and uniform scale.
/// `room::enter_room` hangs `PatchBayRoot` under an entity carrying this.
fn placement_transform(p: &StationPlacement) -> Transform {
    Transform::from_translation(p.translation)
        .with_rotation(Quat::from_rotation_y(p.yaw))
        .with_scale(Vec3::splat(p.scale))
}

/// Map a patch-bay-LOCAL point to room space through a placement — the same
/// similarity transform [`placement_transform`] applies to the subtree, but as
/// a point mapping the camera dolly can read without touching an entity. Pure;
/// unit-tested.
fn placement_to_room(p: &StationPlacement, local: Vec3) -> Vec3 {
    p.translation + Quat::from_rotation_y(p.yaw) * (local * p.scale)
}

/// The dive camera's room-space `(eye, look-at)` — the local [`PB_CAM_POS`] /
/// [`PB_CAM_LOOK`] carried through [`STATION_W_PLACEMENT`]. `ease_shell_camera`
/// glides to this while `Screen::PatchBay`, so descending onto the W furniture
/// frames the table exactly as the standalone scene did (a similarity transform
/// preserves the camera→scene angles, so the baked `face_camera` plate facing
/// still points at the world eye).
pub(crate) fn dive_camera_pose() -> (Vec3, Vec3) {
    (
        placement_to_room(&STATION_W_PLACEMENT, PB_CAM_POS),
        placement_to_room(&STATION_W_PLACEMENT, PB_CAM_LOOK),
    )
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

// ── Components ──────────────────────────────────────────────────────────────

/// The placement entity that re-roots the patch bay into room space at the W
/// bearing (child of `RoomRoot`; carries [`STATION_W_PLACEMENT`]). `PatchBayRoot`
/// is its child, so the whole scene moves as one.
#[derive(Component)]
pub struct StationWPlacement;

#[derive(Component)]
pub struct PatchBayRoot;

/// The dived view's LOD layer — port labels, radial etch ticks, group/title/info
/// plates: the "dedicated view" text+tick detail that appears only on the dive
/// (`docs/scenes/shell.md`, slice B). `apply_patch_lod` shows these while
/// `Screen::PatchBay` and hides them at room scale, where the bare chords over
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
/// the sockets (unlike the static concentric rings spawned in `enter_patch_bay`).
#[derive(Component)]
pub struct EtchTick;

// ── Plugin ──────────────────────────────────────────────────────────────────

pub struct PatchBayPlugin;

impl Plugin for PatchBayPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PatchBayState>()
            .insert_non_send_resource(PatchBayAlsa::default())
            .add_plugins(MaterialPlugin::<ChordMaterial>::default())
            .add_systems(OnEnter(Screen::PatchBay), enter_patch_bay)
            .add_systems(OnExit(Screen::PatchBay), exit_patch_bay)
            .add_systems(
                Update,
                (
                    // Dived-only input; first so a Left/Right selection lands
                    // before the rebuild/fill it feeds.
                    patch_bay_keyboard.run_if(in_state(Screen::PatchBay)),
                    // Ambient truth: the observed graph, its chords, the
                    // selection glow, and traffic pulses stay live across BOTH
                    // shell screens — the chords ARE the W ambient even seen
                    // from room scale (the bearing's promise), so these can't be
                    // gated behind the dive.
                    poll_patch_graph.run_if(in_shell),
                    rebuild_patch_scene.run_if(in_shell),
                    update_wire_selection.run_if(in_shell),
                    // Dived-only detail: the inspection card pose and the text
                    // layer (fills after rebuild spawns the plates it reads;
                    // `fill_patch_text` owns the `text_dirty` clear, so port
                    // labels fill first on the same armed flag).
                    position_info_plate.run_if(in_state(Screen::PatchBay)),
                    fill_port_labels.run_if(in_state(Screen::PatchBay)),
                    fill_patch_text.run_if(in_state(Screen::PatchBay)),
                    pulse_render_chords.run_if(in_shell),
                    // LOD visibility last, so a label/tick rebuild spawned this
                    // frame gets its room-vs-dive visibility set before render.
                    apply_patch_lod.run_if(in_shell),
                )
                    .chain(),
            );
    }
}

// ── Enter / exit ────────────────────────────────────────────────────────────

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

/// Dive into the patch bay. The furniture already stands at W as part of the
/// room ([`spawn_furniture`]), so this is **not** a spawn and claims no camera:
/// the shared camera dollies down onto the table (`ease_shell_camera` retargets
/// the moment the state flips) and the LOD layer appears (`apply_patch_lod`).
/// Re-arm the text so the labels fill on this dive even when the graph is
/// unchanged since the last one (the fill systems run only while dived).
fn enter_patch_bay(mut state: ResMut<PatchBayState>) {
    state.text_dirty = true;
    info!("patch-bay: dived");
}

/// Leave the dive. Two very different exits share this `OnExit`:
///
/// - **Surfacing to the room** (Esc/Up): nothing to tear down — the room and
///   its W furniture survive, `ease_shell_camera` glides back up,
///   `apply_patch_lod` hides the detail layer; the wire selection is left
///   intact so re-diving resumes where it was.
/// - **Leaving the shell from the dive**: a context switch landing while dived
///   reveals the conversation (`view/sync.rs`), an `open_editor` peer signal
///   jumps to the editor. `OnExit(Screen::Room)` will NOT fire on that path —
///   the state being left is `PatchBay` — so the room teardown falls to us
///   ([`teardown_room`]), or the room leaks into every later screen.
///
/// `State<Screen>` already holds the *target* during OnExit (the bevy_state
/// ordering guarantee documented at `room::exit_room`).
fn exit_patch_bay(
    mut commands: Commands,
    screen: Res<State<Screen>>,
    theme: Res<crate::ui::theme::Theme>,
    roots: Query<Entity, With<RoomRoot>>,
    mut app_camera: Query<(Entity, &mut Camera), With<RoomCamera>>,
) {
    if *screen.get() == Screen::Room {
        info!("patch-bay: surfaced");
        return;
    }
    teardown_room(&mut commands, &theme, &roots, &mut app_camera);
    info!("patch-bay: left the shell from the dive (room torn down)");
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
    // room"): `DARK_SURFACE_LIFT` is one shade up from the room's own
    // furniture surface, so the instrument's working face reads against its
    // dais with no lamp needed.
    let table_material = std_materials.add(StandardMaterial {
        base_color: lin(palette::DARK_SURFACE_LIFT),
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
    // One shared unlit material, `palette::GOLD_HUE` at the etch tier (dimmer
    // than trim so etched detail supports rather than competes) — reads at
    // rest and never blooms; `cull_mode: None` keeps the up-facing annulus
    // visible either side.
    let etch_material = std_materials.add(StandardMaterial {
        base_color: lin_scaled(palette::GOLD_HUE, palette::GOLD_LDR_ETCH),
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

/// The dive LOD: show the label/tick/card layer while `Screen::PatchBay`, hide
/// it at room scale (`docs/scenes/shell.md`, slice B — the dived view earns its
/// focus by *showing the labels*, the room ambient is the bare chords over the
/// socket rings). Change-guarded so a settled entity never re-dirties; it also
/// corrects the labels/ticks `rebuild_patch_scene` spawns this frame (they spawn
/// `Hidden`, so the room-scale default needs no fix — only the dive shows them).
fn apply_patch_lod(
    screen: Res<State<Screen>>,
    mut lod: Query<&mut Visibility, With<PatchBayLod>>,
) {
    let want = if *screen.get() == Screen::PatchBay {
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

/// Keys: Left/Right cycle wires; Up/Esc go up to the room; `r` rescans now.
fn patch_bay_keyboard(
    keys: Res<ButtonInput<KeyCode>>,
    mut state: ResMut<PatchBayState>,
    mut alsa: NonSendMut<PatchBayAlsa>,
    mut room: ResMut<RoomState>,
    mut next: ResMut<NextState<Screen>>,
) {
    let n = state.snapshot.wires.len();
    if n > 0 {
        if keys.just_pressed(KeyCode::ArrowRight) || keys.just_pressed(KeyCode::Tab) {
            state.selected = (state.selected + 1) % n;
            state.text_dirty = true;
        } else if keys.just_pressed(KeyCode::ArrowLeft) {
            state.selected = (state.selected + n - 1) % n;
            state.text_dirty = true;
        }
    }

    if keys.just_pressed(KeyCode::KeyR) {
        let elapsed = state.timer.duration();
        state.timer.set_elapsed(elapsed);
        // A failed open otherwise latches forever (`poll_patch_graph`'s
        // `get_or_insert_with` never runs its closure again); an explicit
        // rescan is the one path allowed to retry it.
        alsa.clear_failed_open();
    }

    if keys.just_pressed(KeyCode::ArrowUp) || keys.just_pressed(KeyCode::Escape) {
        room.carousel = StationCarousel::new(Station::PatchBay);
        next.set(Screen::Room);
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
        base_color: lin_scaled(palette::BRASS_HUE, palette::BRASS_LDR),
        unlit: true,
        ..default()
    });
    let tick_material = std_materials.add(StandardMaterial {
        base_color: lin_scaled(palette::GOLD_HUE, palette::GOLD_LDR_ETCH),
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
        // the peg and facing the fixed camera. Text is committed once the font
        // loads by `fill_port_labels`.
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
                face_camera(pos),
                Visibility::Hidden,
                panel,
                Name::new("PortLabel"),
                ChildOf(root),
            ));
        }
    }

    // Group nameplates: the supporting layer — dimmer and further out than the
    // port labels, facing the fixed camera. `layout_sockets` emits exactly one
    // label per group, same order as `groups`.
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
        let mesh = meshes.add(Rectangle::new(150.0, 44.0));
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
            face_camera(pos),
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
            color: Vec4::new(WIRE_HUE.red, WIRE_HUE.green, WIRE_HUE.blue, 1.0),
            // params.y = PULSE_IDLE: no packet until the pulse system stamps a
            // send. tune carries the Amy-tunable pulse shape into the shader.
            params: Vec4::new(if selected { 1.0 } else { 0.0 }, PULSE_IDLE, 0.0, 0.0),
            tune: Vec4::new(PULSE_TRAVEL_SECS, PULSE_BAND_WIDTH, PULSE_GAIN, CHORD_SELECTED_GAIN),
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
/// lifted), facing the fixed camera — recomputed whenever the selection or the
/// graph changes. With no drawable chord (no wires, or an endpoint filtered
/// away) it falls back to the edge pose the "NO WIRES" text lives at.
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
    // Camera is static — orient once here, no per-frame billboarding. `looking_at`
    // a point mirrored past the plate turns its +Z face toward the camera (same
    // idiom as `plate_bundle`).
    *tf = Transform::from_translation(target).looking_at(target * 2.0 - PB_CAM_POS, Vec3::Y);
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

/// Orient a plate at `pos` so its readable face points at the **fixed**
/// patch-bay camera. `PB_CAM_POS` never moves in this scene, so we bake the
/// facing once at spawn — no per-frame billboard system. A `Rectangle` faces
/// +Z; aiming its forward (-Z) at `2·pos − PB_CAM_POS` swings +Z back toward
/// the eye, keeping every rim plate square-on instead of edge-on at tangent
/// angles (the unreadable-nameplate fix, `docs/scenes/patchbay.md`).
fn face_camera(pos: Vec3) -> Transform {
    Transform::from_translation(pos).looking_at(pos * 2.0 - PB_CAM_POS, Vec3::Y)
}

/// A floating MSDF text plate facing the patch-bay camera (borderless,
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
        face_camera(pos),
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
    fn the_placement_seam_shrinks_a_local_height_by_the_scale_alone() {
        // A point straight up in patch-bay-local space rises `scale`× as high
        // above the station and moves nowhere in XZ (yaw is about +Y, so it
        // can't tilt a vertical) — the uniform-scale-plus-rigid seam.
        let p = &STATION_W_PLACEMENT;
        let up = placement_to_room(p, Vec3::new(0.0, 100.0, 0.0));
        assert!((up.y - (p.translation.y + 100.0 * p.scale)).abs() < 1e-3, "{up:?}");
        assert!((up.x - p.translation.x).abs() < 1e-3, "{up:?}");
        assert!((up.z - p.translation.z).abs() < 1e-3, "{up:?}");
    }

    #[test]
    fn the_dive_camera_leans_down_onto_the_table_from_the_approach_side() {
        // The dive pose is the local look-down camera carried through the seam:
        // the eye rides above the dais-top height and above its own look
        // point (tilting down onto the top face), and it stands on the +X
        // (console) side of the table center — the same side the room's W-focus
        // camera studies it from, so the descent is a lean-in, not a jump across.
        let (eye, look) = dive_camera_pose();
        let t = STATION_W_PLACEMENT.translation;
        assert!(eye.y > t.y, "dive eye sits above the dais top: {eye:?}");
        assert!(eye.y > look.y, "camera tilts down onto the table: eye={eye:?} look={look:?}");
        assert!(eye.x > t.x, "dive eye is on the approach (+X) side of the table: {eye:?}");
    }

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

    // -- shell lifecycle: leaving from a dive (shared scene graph) -------
    //
    // With one shared scene graph, `OnExit(Screen::Room)` does NOT fire on a
    // transition that leaves the shell FROM the dived screen (PatchBay →
    // Conversation/Editor — a context switch revealing the conversation, an
    // `open_editor` peer signal). These app-level tests drive the real state
    // machine with only the two exit systems registered (the enter systems
    // need the full render stack; the lifecycle contract is what's under
    // test) and a hand-stood room: RoomRoot + a claimed camera.

    fn shell_lifecycle_app() -> App {
        let mut app = App::new();
        app.add_plugins(bevy::state::app::StatesPlugin)
            .insert_resource(crate::ui::theme::Theme::default())
            .init_state::<Screen>()
            .add_systems(OnExit(Screen::Room), crate::view::room::exit_room)
            .add_systems(OnExit(Screen::PatchBay), exit_patch_bay);
        app
    }

    fn set_screen(app: &mut App, s: Screen) {
        app.world_mut().resource_mut::<NextState<Screen>>().set(s);
        app.update();
    }

    fn room_root_count(app: &mut App) -> usize {
        app.world_mut()
            .query_filtered::<Entity, With<RoomRoot>>()
            .iter(app.world())
            .count()
    }

    #[test]
    fn leaving_the_shell_from_a_dive_tears_the_room_down() {
        let mut app = shell_lifecycle_app();
        app.world_mut().spawn(RoomRoot);
        let cam = app.world_mut().spawn((Camera::default(), RoomCamera)).id();

        set_screen(&mut app, Screen::Room);
        set_screen(&mut app, Screen::PatchBay);
        assert_eq!(room_root_count(&mut app), 1, "the dive keeps the room alive");

        // The leak path: a context switch / open_editor yanks the screen
        // straight out of the dive, bypassing OnExit(Room).
        set_screen(&mut app, Screen::Conversation);
        assert_eq!(room_root_count(&mut app), 0, "exit_patch_bay must tear the room down");
        assert!(
            app.world().get::<RoomCamera>(cam).is_none(),
            "the camera claim must be released"
        );
        let theme_bg = app.world().resource::<crate::ui::theme::Theme>().bg;
        let clear = app.world().get::<Camera>(cam).unwrap().clear_color;
        assert!(
            matches!(clear, ClearColorConfig::Custom(c) if c == theme_bg),
            "the conversation clear colour must be restored: {clear:?}"
        );
    }

    #[test]
    fn surfacing_from_a_dive_keeps_the_room_and_the_camera_claim() {
        let mut app = shell_lifecycle_app();
        app.world_mut().spawn(RoomRoot);
        let cam = app.world_mut().spawn((Camera::default(), RoomCamera)).id();

        set_screen(&mut app, Screen::Room);
        set_screen(&mut app, Screen::PatchBay);
        set_screen(&mut app, Screen::Room);
        assert_eq!(room_root_count(&mut app), 1, "surfacing is travel, not teardown");
        assert!(
            app.world().get::<RoomCamera>(cam).is_some(),
            "the shared camera stays claimed across the whole shell visit"
        );
    }

    #[test]
    fn leaving_the_room_itself_still_tears_down() {
        // The pre-existing exit_room path — locked so the refactor into
        // `teardown_room` can't have changed it.
        let mut app = shell_lifecycle_app();
        app.world_mut().spawn(RoomRoot);
        let cam = app.world_mut().spawn((Camera::default(), RoomCamera)).id();

        set_screen(&mut app, Screen::Room);
        set_screen(&mut app, Screen::Conversation);
        assert_eq!(room_root_count(&mut app), 0);
        assert!(app.world().get::<RoomCamera>(cam).is_none());
    }
}
