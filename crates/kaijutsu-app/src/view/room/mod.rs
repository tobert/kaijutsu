//! Room level — the station-carousel blockout (`docs/scenes/shell.md`).
//!
//! Slice 0 of the shell: not the Tardis room yet, just its *navigation
//! skeleton* — a row of engraved-style station nameplates on the shared 3D
//! camera. Left/Right cycle the focus, Enter/Down dives into a built station
//! (well, patch bay), Esc drops back to the conversation. Reached from the
//! well by Up-Up at the mouth ring (the speedbumped edge —
//! [`WellEdgeBump`]). The full room (bearings, trace floor, radiators)
//! replaces these visuals later without changing the keys.

pub mod nav;

use bevy::prelude::*;

use nav::{DoubleTap, Station, StationCarousel};

use crate::shaders::WellCardMaterial;
use crate::text::ShapingFonts;
use crate::text::components::bevy_color_to_brush;
use crate::text::msdf::{
    FontDataMap, MsdfAtlas, MsdfBlockGlyphs, PositionedGlyph, collect_msdf_glyphs,
};
use crate::text::shaping::{VelloFont, VelloTextAlign, VelloTextStyle};
use crate::ui::screen::Screen;
use crate::view::time_well::panel::{commit_panel_glyphs, create_msdf_panel};
use vello::peniko::Brush;

// ── Blockout constants (Amy-tunable) ───────────────────────────────────────

/// Room background — a shade darker than the well's, so leaving the well
/// upward reads as stepping back into the larger dark of the room.
const ROOM_BG: Color = Color::srgb(0.030, 0.036, 0.058);

/// Nameplate quad (world units) and its texture (logical px).
const PLATE_QUAD_W: f32 = 210.0;
const PLATE_QUAD_H: f32 = 62.0;
pub(crate) const PLATE_TEX_W: f32 = 340.0;
pub(crate) const PLATE_TEX_H: f32 = 100.0;
pub(crate) const PLATE_PAD: f32 = 10.0;
pub(crate) const PLATE_FONT_SIZE: f32 = 30.0;

/// Row layout: plates on a shallow arc facing the camera.
const PLATE_SPACING: f32 = 250.0;
const PLATE_Y: f32 = 60.0;
/// Camera pose for the room blockout.
const ROOM_CAM_POS: Vec3 = Vec3::new(0.0, 110.0, 640.0);
const ROOM_CAM_LOOK: Vec3 = Vec3::new(0.0, 55.0, 0.0);

/// Focus presentation: focused plates brighten + grow; unbuilt stations stay
/// dim even when focused (they're placeholders on the carousel).
const PLATE_DIM_FOCUSED: f32 = 1.0;
const PLATE_DIM_IDLE: f32 = 0.45;
const PLATE_DIM_UNBUILT: f32 = 0.22;
const PLATE_SCALE_FOCUSED: f32 = 1.18;
/// Brass border for the focused plate (the engraved-nameplate read).
const PLATE_BORDER_FOCUSED: Vec4 = Vec4::new(0.85, 0.65, 0.25, 1.0);
/// Plate body fill.
const PLATE_ACCENT: Vec4 = Vec4::new(0.075, 0.085, 0.125, 1.0);

/// The well-edge speedbump window (ms) — same 500ms as the app's other
/// double-tap gestures (`input/interrupt.rs`, `input/vim/dismiss.rs`).
const EDGE_BUMP_WINDOW_MS: u128 = 500;

// ── State ───────────────────────────────────────────────────────────────────

/// Which station the room carousel focuses. Whoever *enters* the room sets
/// the focus first (the well focuses TIME WELL; the patch bay focuses PATCH
/// BAY), so arriving always faces where you came from.
#[derive(Resource)]
pub struct RoomState {
    pub carousel: StationCarousel,
}

impl Default for RoomState {
    fn default() -> Self {
        Self { carousel: StationCarousel::new(Station::TimeWell) }
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

// ── Components ──────────────────────────────────────────────────────────────

/// Root of all room blockout entities (despawn is recursive).
#[derive(Component)]
pub struct RoomRoot;

/// Marks the shared app camera while the room owns it.
#[derive(Component)]
pub struct RoomCamera;

/// One station nameplate; `0` indexes [`Station::ALL`].
#[derive(Component)]
pub struct StationPlate(pub usize);

// ── Plugin ──────────────────────────────────────────────────────────────────

pub struct RoomPlugin;

impl Plugin for RoomPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RoomState>()
            .init_resource::<WellEdgeBump>()
            .add_systems(OnEnter(Screen::Room), enter_room)
            .add_systems(OnExit(Screen::Room), exit_room)
            .add_systems(
                Update,
                (room_keyboard, room_plate_text, room_focus_visuals)
                    .chain()
                    .run_if(in_state(Screen::Room)),
            );
    }
}

// ── Enter / exit ────────────────────────────────────────────────────────────

fn enter_room(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<WellCardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut app_camera: Query<(Entity, &mut Camera, &mut Transform), With<Camera3d>>,
) {
    // Claim the shared app camera (the well-marker convention: insert to
    // claim, remove + restore clear color to release).
    if let Ok((cam_entity, mut cam, mut tf)) = app_camera.single_mut() {
        commands.entity(cam_entity).insert(RoomCamera);
        cam.clear_color = ClearColorConfig::Custom(ROOM_BG);
        *tf = Transform::from_translation(ROOM_CAM_POS).looking_at(ROOM_CAM_LOOK, Vec3::Y);
    }

    let root = commands
        .spawn((RoomRoot, Transform::default(), Visibility::Inherited, Name::new("RoomRoot")))
        .id();

    let half = (Station::ALL.len() as f32 - 1.0) / 2.0;
    for (i, station) in Station::ALL.iter().enumerate() {
        let x = (i as f32 - half) * PLATE_SPACING;
        // Shallow arc: outer plates sit slightly deeper.
        let z = -0.06 * x.abs();
        let mesh = meshes.add(Rectangle::new(PLATE_QUAD_W, PLATE_QUAD_H));
        let (image, panel) =
            create_msdf_panel(&mut images, PLATE_TEX_W as u32, PLATE_TEX_H as u32);
        let material = materials.add(WellCardMaterial {
            texture: image,
            accent: PLATE_ACCENT,
            params: Vec4::ZERO,
            shape: plate_shape(),
            border: Vec4::ZERO,
            // dim.x only — .y/.z are the live chatter/beat lanes (leaving
            // them nonzero paints the accidental cyan+gold ring).
            dim: Vec4::new(PLATE_DIM_IDLE, 0.0, 0.0, 0.0),
        });
        let pos = Vec3::new(x, PLATE_Y, z);
        // Face the camera: look from the plate *away* from the camera point
        // (Rectangle faces +Z, looking_at points -Z at the target).
        commands.spawn((
            StationPlate(i),
            Mesh3d(mesh),
            MeshMaterial3d(material),
            Transform::from_translation(pos).looking_at(pos * 2.0 - ROOM_CAM_POS, Vec3::Y),
            Visibility::Inherited,
            panel,
            Name::new(format!("StationPlate-{}", station.label())),
            ChildOf(root),
        ));
    }

    info!("room: entered (blockout carousel)");
}

fn exit_room(
    mut commands: Commands,
    theme: Res<crate::ui::theme::Theme>,
    roots: Query<Entity, With<RoomRoot>>,
    mut app_camera: Query<(Entity, &mut Camera), With<RoomCamera>>,
) {
    for e in roots.iter() {
        commands.entity(e).despawn();
    }
    if let Ok((cam_entity, mut cam)) = app_camera.single_mut() {
        commands.entity(cam_entity).remove::<RoomCamera>();
        cam.clear_color = ClearColorConfig::Custom(theme.bg);
    }
    info!("room: exited");
}

// ── Systems ─────────────────────────────────────────────────────────────────

/// Room keys: Left/Right cycle the carousel, Enter/Down dive into a built
/// station, Esc drops to the conversation (the room is the top level).
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

/// Fill the nameplates once the mono font is ready (the same async-font gate
/// as the well's label builders).
fn room_plate_text(
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    mut plates: Query<(&StationPlate, &mut MsdfBlockGlyphs)>,
    mut done: Local<bool>,
) {
    if *done {
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
    // Plates spawn via Commands the frame before they're queryable — only
    // latch once we actually filled something.
    *done = any;
}

/// Focus presentation: brighten + grow the focused plate, brass-frame it;
/// unbuilt stations stay dim even focused.
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
        tf.scale = Vec3::splat(scale);
        if let Some(mat) = materials.get_mut(&mat_handle.0) {
            mat.dim.x = dim;
            mat.border = if focused { PLATE_BORDER_FOCUSED } else { Vec4::ZERO };
        }
    }
}

// ── Shared plate-text helper (also used by the patch bay's plates) ─────────

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

/// `WellCardMaterial.shape` for a nameplate: texture aspect, soft corner,
/// thin border channel (drawn only when `border` is nonzero).
fn plate_shape() -> Vec4 {
    Vec4::new(PLATE_TEX_W / PLATE_TEX_H, 0.10, 0.05, 0.0)
}
