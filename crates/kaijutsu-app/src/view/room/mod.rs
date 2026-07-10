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
//! - **Information radiators**: violet dark-glass panels between bearings
//!   (idle/LDR placeholders for slice A).
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
/// Dark floor colour (linear rgb).
const FLOOR_COLOR: [f32; 3] = [0.016, 0.020, 0.032];

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
/// Radius of the information-radiator panels (the between-station walls).
const RADIATOR_RADIUS: f32 = 660.0;

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

/// Nameplate quad (world units) and its texture (logical px) — the well's plate
/// grammar, kept API-compatible for the patch bay's own plates.
const PLATE_QUAD_W: f32 = 210.0;
const PLATE_QUAD_H: f32 = 62.0;
pub(crate) const PLATE_TEX_W: f32 = 340.0;
pub(crate) const PLATE_TEX_H: f32 = 100.0;
pub(crate) const PLATE_PAD: f32 = 10.0;
pub(crate) const PLATE_FONT_SIZE: f32 = 30.0;
/// Height (world-Y) the nameplates float at.
const PLATE_HEIGHT: f32 = 150.0;

// ── Console emblem (gold — the well's reserved hue) ──────────────────────────

/// The console: a stack of gold rings at center — the slice-A stand-in for the
/// live well. `(y, major_radius, minor_radius)` per ring, apex smallest.
const CONSOLE_RINGS: [(f32, f32, f32); 3] =
    [(14.0, 96.0, 7.0), (36.0, 68.0, 6.0), (56.0, 42.0, 5.0)];
/// Console gold hue (linear rgb identity).
const CONSOLE_GOLD_HUE: [f32; 3] = [1.00, 0.78, 0.34];
/// Console rest brightness (LDR — a soft steady glow, no bloom).
const CONSOLE_LDR: f32 = 0.60;
/// Chatter gain: `activity(Center)` (0..1) → this much HDR lift on the console.
const CONSOLE_CHATTER_GAIN: f32 = 2.2;

// ── Information radiators (violet — reserved for information) ─────────────────

/// Radiator panel dimensions (a tall slim slab of dark glass).
const RADIATOR_WIDTH: f32 = 92.0;
const RADIATOR_HEIGHT: f32 = 430.0;
const RADIATOR_DEPTH: f32 = 10.0;
const RADIATOR_HEIGHT_OFFSET: f32 = 40.0;
/// Idle radiator colour (linear rgb) — a dim violet dark-glass, LDR only for
/// slice A (no live content yet).
const RADIATOR_COLOR: [f32; 3] = [0.090, 0.040, 0.150];

// ── Floor traces (the wiring — static LDR engravings for slice A) ────────────

/// Trace ribbon height above the floor (avoids z-fighting) and base width.
const TRACE_Y: f32 = 0.6;
const TRACE_WIDTH: f32 = 7.0;
/// Etched fabric hues (linear rgb, dim): crimson = MIDI, cyan = PCM. At rest a
/// trace is a dark engraving; it lights HDR only when its flow runs (later
/// slices). One hue family per fabric (the charter's rainbow-board rule).
const TRACE_CRIMSON: [f32; 3] = [0.24, 0.055, 0.070];
const TRACE_CYAN: [f32; 3] = [0.050, 0.170, 0.210];

/// Trace arcs: `(radius, start_deg, end_deg, hue, width_scale)`. Every radius is
/// outside [`KEEPOUT_RADIUS`], so each concentric arc bows around the console.
const TRACE_ARCS: [(f32, f32, f32, [f32; 3], f32); 5] = [
    (470.0, 200.0, 344.0, TRACE_CRIMSON, 1.0),
    (360.0, 22.0, 168.0, TRACE_CYAN, 1.0),
    (250.0, 250.0, 340.0, TRACE_CYAN, 0.7),
    (300.0, 40.0, 130.0, TRACE_CRIMSON, 0.7),
    (410.0, 10.0, 70.0, TRACE_CRIMSON, 0.6),
];

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
/// The console (TimeWell) overview pose — elevated and pulled back from the
/// south, framing the *whole* room so every bearing's ambient glow reads at
/// once (the tracks (E) marker must breathe here without diving — the slice-A
/// acceptance). **Amy-tunable** (the lead live-tunes the exact framing).
const OVERVIEW_POS: Vec3 = Vec3::new(0.0, 640.0, 1500.0);
const OVERVIEW_LOOK: Vec3 = Vec3::new(0.0, 40.0, 0.0);
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

    // Floor disc — dark, flat (Circle meshes in XY facing +Z; tip it to lie in
    // the XZ floor plane facing up).
    let floor_mesh = meshes.add(Circle::new(FLOOR_RADIUS));
    let floor_mat = mats.add(unlit(lin(FLOOR_COLOR)));
    commands.spawn((
        Mesh3d(floor_mesh),
        MeshMaterial3d(floor_mat),
        Transform::from_rotation(Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2)),
        Visibility::Inherited,
        Name::new("RoomFloor"),
        ChildOf(root),
    ));

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

    // Console emblem — a stack of gold rings at center (the well's stand-in).
    // One shared material so the chatter glow lifts them together.
    let console_mat = mats.add(unlit(lin_scaled(CONSOLE_GOLD_HUE, CONSOLE_LDR)));
    for (i, (y, major, minor)) in CONSOLE_RINGS.iter().enumerate() {
        commands.spawn((
            Mesh3d(meshes.add(Torus { minor_radius: *minor, major_radius: *major })),
            MeshMaterial3d(console_mat.clone()),
            Transform::from_xyz(0.0, *y, 0.0),
            Visibility::Inherited,
            ConsoleEmblem,
            RoomDistraction,
            Name::new(format!("ConsoleRing{i}")),
            ChildOf(root),
        ));
    }

    // Floor traces — static LDR engravings that bow around the console.
    for (radius, a0, a1, hue, wscale) in TRACE_ARCS {
        debug_assert!(
            radius > KEEPOUT_RADIUS,
            "trace radius {radius} must clear the console keep-out ({KEEPOUT_RADIUS})"
        );
        let pts = bearing::arc_points(radius, a0.to_radians(), a1.to_radians(), 48, TRACE_Y);
        let mesh = meshes.add(ribbon_mesh(&pts, TRACE_WIDTH * wscale));
        let mat = mats.add(StandardMaterial {
            base_color: lin(hue),
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
    }

    // Wall stations: a marker pylon at each bearing, plus an engraved nameplate
    // at the labeled ones (the reserved South bearing gets a dim marker only).
    for wp in bearing::wall_placements() {
        let hue = Vec3::from_array(wp.hue);
        // The reserved South bearing (no station) gets a low stub — tall
        // enough to read as "reserved", short enough to stop standing in the
        // overview shot's near foreground (MARKER_HEIGHT_RESERVED).
        let marker_h = if wp.station.is_some() { MARKER_HEIGHT } else { MARKER_HEIGHT_RESERVED };
        let marker_mesh = meshes.add(Cuboid::new(MARKER_WIDTH, marker_h, MARKER_WIDTH));
        let marker_mat = mats.add(unlit(lin_v(hue * MARKER_LDR)));
        let marker_pos = Vec3::new(wp.dir[0] * ROOM_RADIUS, marker_h * 0.5, wp.dir[2] * ROOM_RADIUS);
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

    // Information radiators — tall violet dark-glass panels between bearings,
    // idle placeholders (no live content in slice A).
    for (i, d) in bearing::RADIATOR_DIRS.iter().enumerate() {
        let mesh = meshes.add(Cuboid::new(RADIATOR_WIDTH, RADIATOR_HEIGHT, RADIATOR_DEPTH));
        let mat = mats.add(unlit(lin(RADIATOR_COLOR)));
        let pos = Vec3::new(
            d[0] * RADIATOR_RADIUS,
            RADIATOR_HEIGHT * 0.5 + RADIATOR_HEIGHT_OFFSET,
            d[2] * RADIATOR_RADIUS,
        );
        // Present the broad face inward (the cuboid's ±Z face is width×height).
        let outward = Vec3::new(pos.x * 2.0, pos.y, pos.z * 2.0);
        commands.spawn((
            Mesh3d(mesh),
            MeshMaterial3d(mat),
            Transform::from_translation(pos).looking_at(outward, Vec3::Y),
            RoomDistraction,
            Visibility::Inherited,
            Name::new(format!("Radiator{i}")),
            ChildOf(root),
        ));
    }

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
/// brass-frame it; unbuilt stations stay dim even focused.
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
        Some(d) => (
            Vec3::from_array(bearing::approach_camera(
                d,
                ROOM_CAM_APPROACH_R,
                ROOM_CAM_APPROACH_HEIGHT,
            )),
            // Look at the marker's own wall radius (ROOM_RADIUS — the same
            // radius the pylons spawn at) held at nameplate height, so the
            // plate is framed square-on.
            Vec3::from_array(bearing::approach_look(d, ROOM_RADIUS, PLATE_HEIGHT)),
        ),
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
                    .map(|p| bearing::dome_color((p[1] / radius) * 0.5 + 0.5))
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

    #[test]
    fn every_floor_trace_bows_around_the_console_keepout() {
        for (radius, _, _, _, _) in TRACE_ARCS {
            assert!(radius > KEEPOUT_RADIUS, "trace at r={radius} would cross the console");
        }
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
