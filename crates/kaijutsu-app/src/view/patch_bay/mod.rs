//! Patch bay station — the circle scene, slice 0 (`docs/scenes/patchbay.md`).
//!
//! Read-only observed reality: the local ALSA seq graph rendered as a round
//! table — brass socket pegs seated around the rim grouped by client, chords
//! of emissive light bowing around the open center for every live
//! subscription. Polled every couple of seconds; hand-run `aconnect` changes
//! appear on the next poll. No write path of any kind (patching stays
//! CLI-only for a long time — the scene is a viewer).
//!
//! Keys: Left/Right cycle the selected wire (the inspection plate follows),
//! Up or Esc returns to the room, `r` forces a poll.

pub mod geometry;

use bevy::prelude::*;

use crate::patch_graph::{PatchGraphReader, PatchGraphSnapshot, diff, without_plumbing};
use crate::shaders::WellCardMaterial;
use crate::text::ShapingFonts;
use crate::text::msdf::{FontDataMap, MsdfAtlas, MsdfBlockGlyphs};
use crate::text::shaping::VelloFont;
use crate::ui::screen::Screen;
use crate::view::room::nav::Station;
use crate::view::room::{RoomState, layout_plate_text};
use crate::view::time_well::panel::{commit_panel_glyphs, create_msdf_panel};
use crate::view::room::nav::StationCarousel;
use geometry::{chord_points, layout_sockets};

// ── Scene constants (Amy-tunable) ───────────────────────────────────────────

const PB_BG: Color = Color::srgb(0.028, 0.034, 0.055);

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

/// Camera pose: tilted look down onto the table.
const PB_CAM_POS: Vec3 = Vec3::new(0.0, 470.0, 590.0);
const PB_CAM_LOOK: Vec3 = Vec3::new(0.0, 0.0, -40.0);

/// Wire palette — crimson = MIDI fabric (`docs/scenes/patchbay.md`, wire
/// grammar). Emissive is the HDR path: selected wires bloom, idle ones glow.
const WIRE_EMISSIVE_IDLE: LinearRgba = LinearRgba::rgb(1.4, 0.16, 0.24);
const WIRE_EMISSIVE_SELECTED: LinearRgba = LinearRgba::rgb(5.5, 0.9, 1.1);

/// Poll cadence for the observed graph.
const POLL_SECS: f32 = 2.0;

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

#[derive(Component)]
pub struct PatchBayRoot;

#[derive(Component)]
pub struct PatchBayCamera;

/// A chord entity; the index into `PatchBayState.snapshot.wires`.
#[derive(Component)]
pub struct ChordWire(pub usize);

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

// ── Plugin ──────────────────────────────────────────────────────────────────

pub struct PatchBayPlugin;

impl Plugin for PatchBayPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<PatchBayState>()
            .insert_non_send_resource(PatchBayAlsa::default())
            .add_systems(OnEnter(Screen::PatchBay), enter_patch_bay)
            .add_systems(OnExit(Screen::PatchBay), exit_patch_bay)
            .add_systems(
                Update,
                (
                    patch_bay_keyboard,
                    poll_patch_graph,
                    rebuild_patch_scene,
                    update_wire_selection,
                    fill_patch_text,
                )
                    .chain()
                    .run_if(in_state(Screen::PatchBay)),
            );
    }
}

// ── Enter / exit ────────────────────────────────────────────────────────────

/// Arm a fresh poll + full scene/text rebuild for the next frame. `PatchBayState`
/// (and its `snapshot`) is a `Resource` — it survives `exit_patch_bay`, which
/// only despawns `PatchBayRoot` and its children. Without forcing `scene_dirty`
/// here, a re-entry where the ALSA graph hasn't changed since the last visit
/// produces an empty `diff` in `poll_patch_graph`, and `rebuild_patch_scene`
/// never runs: a bare table forever, even though `state.snapshot` is valid.
fn arm_on_enter(state: &mut PatchBayState) {
    let full = state.timer.duration();
    state.timer.set_elapsed(full);
    state.text_dirty = true;
    state.scene_dirty = true;
}

fn enter_patch_bay(
    mut commands: Commands,
    mut state: ResMut<PatchBayState>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut std_materials: ResMut<Assets<StandardMaterial>>,
    mut card_materials: ResMut<Assets<WellCardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut app_camera: Query<(Entity, &mut Camera, &mut Transform), With<Camera3d>>,
) {
    if let Ok((cam_entity, mut cam, mut tf)) = app_camera.single_mut() {
        commands.entity(cam_entity).insert(PatchBayCamera);
        cam.clear_color = ClearColorConfig::Custom(PB_BG);
        *tf = Transform::from_translation(PB_CAM_POS).looking_at(PB_CAM_LOOK, Vec3::Y);
    }

    // Force a fresh poll + rebuild on every entry.
    arm_on_enter(&mut state);

    let root = commands
        .spawn((
            PatchBayRoot,
            Transform::default(),
            Visibility::Inherited,
            Name::new("PatchBayRoot"),
        ))
        .id();

    // The table: a flat annulus — the hole in the middle IS the open-center
    // rule, built into the furniture. Extrusions extrude along Z; rotate to
    // lie flat with the top face up.
    let table_mesh = meshes.add(Extrusion::new(Annulus::new(TABLE_INNER_R, TABLE_OUTER_R), TABLE_DEPTH));
    let table_material = std_materials.add(StandardMaterial {
        base_color: Color::srgb(0.085, 0.09, 0.11),
        metallic: 0.85,
        perceptual_roughness: 0.42,
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

    // StandardMaterial needs light (the well's custom shaders don't; this
    // scene has real metal). One warm point light high over the table.
    commands.spawn((
        PointLight {
            intensity: 60_000_000.0,
            range: 4000.0,
            shadows_enabled: false,
            color: Color::srgb(1.0, 0.92, 0.78),
            ..default()
        },
        Transform::from_xyz(0.0, 700.0, 200.0),
        Name::new("PatchBayLight"),
        ChildOf(root),
    ));

    // Title, legend, inspection plates (MSDF; filled by `fill_patch_text`).
    let title = plate_bundle(
        &mut meshes,
        &mut card_materials,
        &mut images,
        Vec3::new(0.0, 150.0, -TABLE_OUTER_R - 60.0),
        1.4,
    );
    commands.spawn((TitlePlate("PATCH BAY"), title, Name::new("PatchBayTitle"), ChildOf(root)));

    let legend = plate_bundle(
        &mut meshes,
        &mut card_materials,
        &mut images,
        Vec3::new(0.0, 8.0, TABLE_OUTER_R + 90.0),
        0.9,
    );
    commands.spawn((
        TitlePlate("<- -> WIRE   UP/ESC ROOM   R RESCAN"),
        legend,
        Name::new("PatchBayLegend"),
        ChildOf(root),
    ));

    let info = plate_bundle(
        &mut meshes,
        &mut card_materials,
        &mut images,
        Vec3::new(TABLE_OUTER_R * 0.78, 190.0, TABLE_OUTER_R * 0.35),
        1.2,
    );
    commands.spawn((InfoPlate, info, Name::new("PatchBayInfo"), ChildOf(root)));

    info!("patch-bay: entered");
}

fn exit_patch_bay(
    mut commands: Commands,
    theme: Res<crate::ui::theme::Theme>,
    roots: Query<Entity, With<PatchBayRoot>>,
    mut app_camera: Query<(Entity, &mut Camera), With<PatchBayCamera>>,
) {
    for e in roots.iter() {
        commands.entity(e).despawn();
    }
    if let Ok((cam_entity, mut cam)) = app_camera.single_mut() {
        commands.entity(cam_entity).remove::<PatchBayCamera>();
        cam.clear_color = ClearColorConfig::Custom(theme.bg);
    }
    info!("patch-bay: exited");
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
    mut images: ResMut<Assets<Image>>,
    roots: Query<Entity, With<PatchBayRoot>>,
    old: Query<Entity, Or<(With<ChordWire>, With<SocketPeg>, With<GroupPlate>)>>,
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

    // Sockets: brass pegs at each seat.
    let peg_mesh = meshes.add(Cylinder::new(7.0, 12.0));
    let peg_material = std_materials.add(StandardMaterial {
        base_color: Color::srgb(0.72, 0.55, 0.25),
        metallic: 0.9,
        perceptual_roughness: 0.3,
        emissive: LinearRgba::rgb(0.10, 0.07, 0.02),
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
    }

    // Group nameplates just outside the rim, facing the camera.
    for label in &labels {
        let (s, c) = label.angle.sin_cos();
        let pos = Vec3::new(RIM_R * 1.22 * c, 34.0, RIM_R * 1.22 * s);
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
            dim: Vec4::new(0.8, 0.0, 0.0, 0.0),
        });
        commands.spawn((
            GroupPlate(label.client_name.clone()),
            Mesh3d(mesh),
            MeshMaterial3d(material),
            Transform::from_translation(pos).looking_at(pos * 2.0 - PB_CAM_POS, Vec3::Y),
            Visibility::Inherited,
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
        let material = std_materials.add(StandardMaterial {
            base_color: Color::BLACK,
            emissive: if selected { WIRE_EMISSIVE_SELECTED } else { WIRE_EMISSIVE_IDLE },
            unlit: false,
            cull_mode: None, // visible from both sides; cheap at this count
            ..default()
        });
        commands.spawn((
            ChordWire(wi),
            Mesh3d(mesh),
            MeshMaterial3d(material),
            Transform::from_translation(Vec3::new(0.0, 2.0, 0.0)),
            Visibility::Inherited,
            Name::new(format!("Chord-{wi}")),
            ChildOf(root),
        ));
    }
}

/// Cheap selection update between rebuilds: emissive follows `selected`.
fn update_wire_selection(
    state: Res<PatchBayState>,
    mut std_materials: ResMut<Assets<StandardMaterial>>,
    chords: Query<(&ChordWire, &MeshMaterial3d<StandardMaterial>)>,
) {
    if !state.is_changed() {
        return;
    }
    for (chord, handle) in chords.iter() {
        if let Some(mat) = std_materials.get_mut(&handle.0) {
            mat.emissive = if chord.0 == state.selected {
                WIRE_EMISSIVE_SELECTED
            } else {
                WIRE_EMISSIVE_IDLE
            };
        }
    }
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
        let glyphs = layout_plate_text(&text, font, atlas, &mut font_data_map);
        commit_panel_glyphs(&mut msdf, glyphs);
    }
    state.text_dirty = false;
}

/// "sender:port -> receiver:port" for the inspection plate; empty when no
/// wires exist (a cleared plate, not a placeholder).
fn describe_selection(snapshot: &PatchGraphSnapshot, selected: usize) -> String {
    let Some(wire) = snapshot.wires.get(selected) else {
        return if snapshot.endpoints.is_empty() {
            "NO ALSA GRAPH".to_string()
        } else {
            "NO WIRES".to_string()
        };
    };
    let name = |addr: (i32, i32)| -> String {
        snapshot
            .endpoints
            .iter()
            .find(|e| (e.client_id, e.port_id) == addr)
            .map(|e| format!("{}:{}", e.client_name, e.port_name))
            .unwrap_or_else(|| format!("{}:{}", addr.0, addr.1))
    };
    format!("{} -> {}", name(wire.src), name(wire.dst))
}

// ── Plate + ribbon helpers ──────────────────────────────────────────────────

/// A floating MSDF text plate facing the patch-bay camera (borderless,
/// label-style; text committed later by [`fill_patch_text`]).
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
        Transform::from_translation(pos).looking_at(pos * 2.0 - PB_CAM_POS, Vec3::Y),
        Visibility::Inherited,
        panel,
    )
}

// ── Ribbon mesh ─────────────────────────────────────────────────────────────

/// A flat ribbon (normal +Y) along `points`, `width` across — the chord's
/// body. UV.x runs along the length for a future pulse shader.
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

    let mut indices: Vec<u32> = Vec::with_capacity((points.len() - 1) * 6);
    for i in 0..(points.len() as u32 - 1) {
        let a = i * 2;
        indices.extend_from_slice(&[a, a + 1, a + 2, a + 2, a + 1, a + 3]);
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

    /// The shape `PatchBayState` is left in by `exit_patch_bay`: the resource
    /// (and its snapshot) survive the despawn untouched, but nothing has
    /// re-armed the dirty flags for the next entry yet.
    fn persisted_after_exit() -> PatchBayState {
        PatchBayState {
            snapshot: non_empty_snapshot(),
            selected: 0,
            scene_dirty: false,
            text_dirty: false,
            timer: Timer::from_seconds(POLL_SECS, TimerMode::Repeating),
        }
    }

    // -- arm_on_enter --------------------------------------------------

    #[test]
    fn arm_on_enter_forces_both_dirty_flags_true() {
        let mut state = persisted_after_exit();
        arm_on_enter(&mut state);
        assert!(
            state.scene_dirty,
            "re-entry must force a rebuild even when the graph hasn't changed since the last visit"
        );
        assert!(state.text_dirty);
    }

    #[test]
    fn arm_on_enter_primes_the_timer_to_fire_on_the_next_tick() {
        let mut state = persisted_after_exit();
        arm_on_enter(&mut state);
        assert!(!state.timer.just_finished(), "not finished until it's ticked");
        assert!(state.timer.tick(Duration::from_millis(1)).just_finished());
    }

    #[test]
    fn arm_on_enter_leaves_the_persisted_snapshot_untouched() {
        let mut state = persisted_after_exit();
        let before = state.snapshot.clone();
        arm_on_enter(&mut state);
        assert_eq!(
            state.snapshot, before,
            "rebuild_patch_scene reads the persisted snapshot; arm_on_enter must not touch it"
        );
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
}
