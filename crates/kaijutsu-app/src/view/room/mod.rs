//! The room holds the Time Well, its live score and the south Switchboard.
//! Left and Right turn between stations. Enter at the well brings its rings
//! into reach, and Back returns to the rail. All views share one camera.

pub mod bearing;
pub mod nav;
mod score_band;
mod score_ink;
mod seats;
mod shot;
mod switchboard;

use bevy::prelude::*;

use nav::{Station, StationCarousel};

use crate::shaders::{TraceGlowMaterial, WellCardMaterial};
use crate::ui::screen::Screen;
use crate::view::scene_geometry;
use crate::view::scene_palette::{ScenePalette, lin_scaled};

// ── Room geometry (Amy-tunable) ─────────────────────────────────────────────
// Color/brightness constants moved onto `Res<ScenePalette>`
// (feat/scene-palette-migration) — see `scene_geometry.rs`'s header. What's left
// here is geometry only.

/// Radius of the floor disc under the well and the Switchboard.
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

/// Central keep-out radius — every floor trace stays outside it, so no trace
/// crosses the console (the open-center rule, `shell.md`). Enforced by a
/// `debug_assert` as each trace spawns.
const KEEPOUT_RADIUS: f32 = 150.0;

// ── Well table (Amy: "MORE SOLIDNESS" — heavy furniture, not hologram) ──────

/// The table the console rings hover above: a chunky tabletop, a gold rim,
/// a narrower pedestal, and a wide low plinth grounding it all to the floor.
/// The well seats its rings above this face.
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
pub(crate) const TABLE_PLINTH_RADIUS: f32 = 145.0;
const TABLE_PLINTH_HEIGHT: f32 = 16.0;

/// Height of the optional key light above the console.
const WELL_LIGHT_HEIGHT: f32 = 180.0;
// Table/pedestal/plinth colour (`ScenePalette::table`) and the rim torus's
// gold trim brightness (`ScenePalette::gold` × `ScenePalette::trim`) moved
// onto the palette resource.

// ── Circuit-board floor (the wiring — static LDR engravings; Amy-tunable) ───

/// Trace ribbon height above the floor (avoids z-fighting) and base width
/// (each route scales this by its own `width_scale`, 0.5–1.2). `TRACE_Y` is
/// `pub(crate)`: it is the room's one answer to "how far off the floor does
/// floor decoration sit", and the time well's event-horizon disc
/// (`time_well::scene::RING_DECK_DEPTH`) derives its own clearance from it
/// rather than inventing a second epsilon.
pub(crate) const TRACE_Y: f32 = 0.6;
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
const GOLD_GLOW_RATE: f32 = 0.15;

/// Route-generation shared geometry ([`bearing::expand_bundle`]): the radial
/// length each 45°-ish bend cuts off the lane radius, and the arc's sample
/// density.
const ROUTE_CHAMFER: f32 = 18.0;
const ROUTE_ARC_SEGMENTS: usize = 14;
/// Terminal-pad disc radius range (world units).
const ROUTE_PAD_RADIUS_RANGE: (f32, f32) = (6.0, 14.0);

// ── Faint moving glow (`TraceGlowMaterial`; Amy, 2026-07-10) ────────────────
// The trace and pad troughs are fractions of the authored route brightness.
/// `(min, max)` seconds for one crest to cross a whole route (mode 0 period,
/// `rate = 1 / period`) — jittered per-route from `bearing::BoardRoute`'s
/// `glow_phase01` so the board doesn't pulse in lockstep.
const TRACE_GLOW_PERIOD_RANGE: (f32, f32) = (8.0, 14.0);
/// Terminal-pad breathing rate (mode 1, rad/s) — slow and uniform; pads
/// don't jitter their rate the way traces jitter their period; only phase
/// (from the same hash stream as their trace) varies pad-to-pad.
const PAD_GLOW_RATE: f32 = 0.25;

/// Camera follow rate (exponential smoothing) — matches the well's weighty
/// glide so the shell and the well feel like one instrument.
const CAMERA_EASE_RATE: f32 = 4.0;
const GLOW_STEP: f32 = 1.0 / 64.0;

// ── State ─────────────────────────────────────────────────────────────────────

/// Which station the room carousel focuses. Defaults to the time well, so
/// arriving always faces the console.
#[derive(Resource)]
pub struct RoomState {
    pub carousel: StationCarousel,
    /// The current dive target. Leaving the room clears the dive.
    pub zoomed: Option<Station>,
}

impl Default for RoomState {
    fn default() -> Self {
        Self { carousel: StationCarousel::new(Station::TimeWell), zoomed: None }
    }
}

// ── Components ────────────────────────────────────────────────────────────────

/// Root of all room entities (despawn is recursive).
#[derive(Component)]
pub struct RoomRoot;

/// Marks the shared app camera while the room owns it.
#[derive(Component)]
pub struct RoomCamera;

/// Peer wisps step out of view when the player dives into the well.
#[derive(Component)]
pub struct RoomDistraction;

// ── Plugin ────────────────────────────────────────────────────────────────────

pub struct RoomPlugin;

impl Plugin for RoomPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<RoomState>()
            .init_resource::<switchboard::SwitchboardState>()
            .init_resource::<seats::SeatsState>()
            .add_plugins(MaterialPlugin::<TraceGlowMaterial>::default())
            .add_systems(OnEnter(Screen::Room), enter_room)
            .add_systems(Update, (
                score_band::engrave_score.after(crate::view::heroic::live::sync_room_score),
                score_band::show_score,
            ).chain().run_if(in_state(Screen::Room)))
            .add_systems(OnExit(Screen::Room), (exit_room, seats::clear_seats))
            .add_systems(
                Update,
                (switchboard::ingest_switchboard_events, switchboard::reconcile_switchboard),
            )
            // `.after(InputPhase::Dispatch)`: these consume ActionFired from
            // the central dispatcher and should see this frame's actions, not
            // last frame's.
            .add_systems(
                Update,
                room_keyboard
                    .after(crate::input::InputPhase::Dispatch)
                    .run_if(in_state(Screen::Room)),
            )
            .add_systems(
                Update,
                (ease_shell_camera, apply_room_dive_visibility).run_if(in_state(Screen::Room)),
            )
            .add_systems(
                Update,
                switchboard::sync_switchboard_glow
                    .after(switchboard::reconcile_switchboard)
                    .run_if(in_state(Screen::Room)),
            )
            // Seats at the table: one wisp per attached peer
            // (`connection::peers::PeerRoster`, polled ungated elsewhere —
            // its own plugin). Both halves are gated to the room (a wisp
            // needs `RoomRoot` to parent onto); `sync_seats` runs
            // `.after(reconcile_seats)` so a wisp spawned this frame is
            // ticked/moved the same frame (`seats`'s own module doc).
            .add_systems(
                Update,
                (seats::reconcile_seats, seats::sync_seats)
                    .chain()
                    .run_if(in_state(Screen::Room)),
            );
    }
}

fn enter_room(
    mut commands: Commands,
    room: Res<RoomState>,
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
    mut app_camera: Query<(Entity, &mut Camera, &mut Transform), With<IsDefaultUiCamera>>,
    existing: Query<Entity, With<RoomRoot>>,
) {
    // Defensive belt-and-braces, not a live path today: `OnEnter(Screen::Room)`
    // only fires on an actual `Screen` transition into `Room` (Bevy states
    // no-op a `set()` to the state already active) — so nothing should ever
    // call this while `RoomRoot` is still standing. Kept anyway so a stale
    // root can never get a second one spawned on top of it.
    if !existing.is_empty() {
        return;
    }

    // Claim the shared app camera (the well-marker convention: insert to claim,
    // remove + restore clear color to release) and place it facing the entering
    // focus so there's no first-frame snap before the ease takes over.
    if let Ok((cam_entity, mut cam, mut tf)) = app_camera.single_mut() {
        commands.entity(cam_entity).insert(RoomCamera);
        cam.clear_color = ClearColorConfig::Custom(Color::LinearRgba(palette.bg));
        let (pos, look) = shot::resolve(shot::RoomShot::focused(room.carousel.focused_station()));
        *tf = Transform::from_translation(pos).looking_at(look, Vec3::Y);
        // Teardown removes this per-view light with the room camera claim.
        if palette.lit_chamber {
            commands.entity(cam_entity).insert(AmbientLight {
                color: Color::LinearRgba(palette.ambient),
                brightness: palette.ambient_brightness,
                ..default()
            });
        }
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
    // Vertex colours carry the gradient; viewed from inside → no back-face
    // cull. In lit-chamber mode the dark gradient becomes low albedo so the
    // dome stays a calm dark vault. Off = the unlit path, byte-for-byte.
    let dome_mat = mats.add(if palette.lit_chamber {
        StandardMaterial {
            base_color: Color::WHITE,
            unlit: false,
            perceptual_roughness: palette.chamber_roughness,
            metallic: 0.0,
            reflectance: palette.chamber_reflectance,
            cull_mode: None,
            ..default()
        }
    } else {
        StandardMaterial { base_color: Color::WHITE, unlit: true, cull_mode: None, ..default() }
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

    // The time well itself — room furniture at the Center bearing, seated
    // above the table's top face via `STATION_CENTER_PLACEMENT`; `arm_well`
    // re-arms the per-visit join state.
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

    score_band::spawn(&mut commands, root, &mut meshes, &mut mats, &palette);
    let display = commands.spawn((
        score_band::SwitchboardDisplay, Transform::default(), Visibility::Inherited,
        Name::new("South Switchboard"), ChildOf(root),
    )).id();
    switchboard::spawn_switchboard(&mut commands, display, &mut meshes, &mut mats);

    // OPTIONAL lit-chamber mode: one warm key light at the well/console center,
    // the room's visual heart. A `RoomRoot` child so it despawns with the room;
    // named so it can be tuned live over BRP (intensity/color/range). Shadows
    // off for the spike. Off (the default) spawns no light and the shell stays
    // all-unlit (`docs/scenes/shell.md`, "all-unlit discipline").
    if palette.lit_chamber {
        commands.spawn((
            PointLight {
                color: Color::LinearRgba(palette.key_light),
                intensity: palette.key_light_intensity,
                range: palette.key_light_range,
                shadow_maps_enabled: false,
                ..default()
            },
            Transform::from_xyz(0.0, WELL_LIGHT_HEIGHT, 0.0),
            Visibility::Inherited,
            Name::new("WellKeyLight"),
            ChildOf(root),
        ));
    }

    info!("room: entered (Tardis chamber — the time well at center)");
}

pub(crate) fn exit_room(
    mut commands: Commands,
    mut room: ResMut<RoomState>,
    theme: Res<crate::ui::theme::Theme>,
    roots: Query<Entity, With<RoomRoot>>,
    mut app_camera: Query<(Entity, &mut Camera), With<RoomCamera>>,
    well_legend: Query<Entity, With<crate::view::time_well::legend::WellLegend>>,
) {
    // Diving onto a station is a `RoomState::zoomed` write, not a screen, so
    // `Screen::Room` is the ONLY state this scene graph ever occupies, and
    // every way out of it runs this one exit. Clear the zoom too, so a later
    // re-entry always starts unzoomed rather than inheriting a stale target
    // from a long-despawned visit.
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
/// its furniture) and release the shared camera (drop the [`RoomCamera`]
/// claim, restore the conversation clear colour).
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
        // Drop the lit-chamber ambient override (a no-op when the mode was off
        // and none was inserted) so the shared camera leaves the room clean.
        commands.entity(cam_entity).remove::<AmbientLight>();
        cam.clear_color = ClearColorConfig::Custom(theme.bg);
    }
}

// ── Spawn helpers (called from `enter_room`) ─────────────────────────────────

/// Floor wiring fans out in nine bundles around the well.
fn route_bundles(palette: &ScenePalette) -> [bearing::RouteBundle; 9] {
    use bearing::{Bearing, RouteBundle, dir_theta};
    let west = dir_theta(Bearing::West.dir());
    let east = dir_theta(Bearing::East.dir());
    let north = dir_theta(Bearing::North.dir());
    let south = dir_theta(Bearing::South.dir());
    let [ne, se, sw, nw] = bearing::DIAGONAL_DIRS.map(dir_theta);
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
            pad_range: (scene_geometry::ROOM_APOTHEM - 160.0, scene_geometry::ROOM_APOTHEM - 30.0),
            hue: trace_crimson,
            brightness_range: (0.7, 1.15),
        },
        RouteBundle {
            center_theta: east,
            spread: 0.50,
            count: 7,
            lane_range: (300.0, 650.0),
            arc_range: (0.25, 0.9),
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
        // Vertex colours carry the dark radial gradient; in lit-chamber mode
        // that gradient becomes low albedo (WHITE × dark vertex colour) so the
        // floor stays dark and catches the well's light. Off = the unlit path,
        // byte-for-byte.
        MeshMaterial3d(mats.add(if palette.lit_chamber {
            StandardMaterial {
                base_color: Color::WHITE,
                unlit: false,
                perceptual_roughness: palette.chamber_roughness,
                metallic: 0.0,
                reflectance: palette.chamber_reflectance,
                ..default()
            }
        } else {
            StandardMaterial { base_color: Color::WHITE, unlit: true, ..default() }
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
            let crest = floor_trace_crest(route.hue, route.brightness_scale, palette.crest);
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

/// The well stands on a dark table with a gold rim and a broad foot.
fn spawn_table(
    commands: &mut Commands,
    root: Entity,
    palette: &ScenePalette,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
) {
    // Table body is chamber shell (lit in lit-chamber mode); the gold rim is
    // content-bearing trim and stays unlit + emissive.
    let table_mat = mats.add(chamber_mat(palette, Color::LinearRgba(palette.table)));
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

/// Turn between the two stations, enter the well or leave the room.
pub(crate) fn room_keyboard(
    mut actions: MessageReader<crate::input::ActionFired>,
    mut room: ResMut<RoomState>,
    mut well_state: ResMut<crate::view::time_well::scene::TimeWellState>,
    mut next: ResMut<NextState<Screen>>,
) {
    use crate::input::Action;

    for crate::input::ActionFired { action, context } in actions.read() {
        // Only actions fired under the RoomNav context belong to the
        // carousel — the context stamp (not run-state) is what prevents a
        // buffered cross-frame or zoomed-station action from replaying here.
        if *context != crate::input::InputContext::RoomNav {
            continue;
        }
        match action {
            Action::StepNext => room.carousel.step(1),
            Action::StepPrev => room.carousel.step(-1),
            Action::Activate => {
                let station = room.carousel.focused_station();
                if station == Station::TimeWell {
                    room.zoomed = Some(station);
                    crate::view::time_well::scene::arm_dive(&mut well_state);
                }
                return;
            }
            Action::PopLevel => {
                next.set(Screen::Conversation);
            }
            _ => {}
        }
    }
}

/// Follow the current station or the selected seat in the well.
fn ease_shell_camera(
    time: Res<Time>,
    room: Res<RoomState>,
    well_state: Res<crate::view::time_well::scene::TimeWellState>,
    score: Res<crate::view::heroic::live::RoomScore>,
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
        Some(Station::Switchboard) => shot::RoomShot::Switchboard,
        None if room.carousel.focused_station() == Station::TimeWell
            && score.score_context.is_some() => shot::RoomShot::ScoreOverview,
        None => shot::RoomShot::focused(room.carousel.focused_station()),
    };
    let (pos, look) = shot::resolve(want);
    let desired = Transform::from_translation(pos).looking_at(look, Vec3::Y);
    let alpha = 1.0 - (-CAMERA_EASE_RATE * time.delta_secs()).exp();
    tf.translation = tf.translation.lerp(desired.translation, alpha);
    tf.rotation = tf.rotation.slerp(desired.rotation, alpha);
}

/// Hide the room’s peer wisps during a dive.
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

// ── Material + colour helpers ──────────────────────────────────────────────────

/// An unlit [`StandardMaterial`] carrying its brightness in `base_color` — the
/// room's one emission channel (HDR blooms, LDR reads crisp).
fn unlit(base_color: Color) -> StandardMaterial {
    StandardMaterial { base_color, unlit: true, ..default() }
}

/// Apply the chamber lighting to the table when enabled.
fn chamber_mat(palette: &ScenePalette, base_color: Color) -> StandardMaterial {
    if palette.lit_chamber {
        StandardMaterial {
            base_color,
            unlit: false,
            perceptual_roughness: palette.chamber_roughness,
            metallic: 0.0,
            reflectance: palette.chamber_reflectance,
            ..default()
        }
    } else {
        unlit(base_color)
    }
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

/// Apply the route's gain to the theme color within the room's crest limit.
fn floor_trace_crest(hue: [f32; 3], gain: f32, ceiling: f32) -> Vec4 {
    let color = Vec3::from_array(hue) * gain;
    let peak = color.max_element();
    if peak > ceiling && peak > 0.0 {
        (color * (ceiling / peak)).extend(0.0)
    } else {
        color.extend(0.0)
    }
}

/// Quantize lamp and seat brightness to limit material updates.
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
    if ((cur.red - target.x).abs() > 1e-4
        || (cur.green - target.y).abs() > 1e-4
        || (cur.blue - target.z).abs() > 1e-4)
        && let Some(mut m) = mats.get_mut(handle)
    {
        m.base_color = lin_v(target);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_entry_places_the_score_and_switchboard_in_an_open_room() {
        use bevy::asset::AssetPlugin;
        use bevy::ecs::system::RunSystemOnce;
        use bevy::scene::ScenePlugin;
        use crate::shaders::{TerraceRingMaterial, WellRingsMaterial};

        let mut app = App::new();
        app.add_plugins((bevy::app::TaskPoolPlugin::default(), AssetPlugin::default(), ScenePlugin))
            .init_asset::<Mesh>()
            .init_asset::<Image>()
            .init_resource::<ScenePalette>()
            .init_resource::<RoomState>()
            .init_resource::<crate::view::time_well::scene::TimeWellState>()
            .init_resource::<crate::view::time_well::rays::WellTracks>()
            .init_resource::<Assets<StandardMaterial>>()
            .init_resource::<Assets<TraceGlowMaterial>>()
            .init_resource::<Assets<WellCardMaterial>>()
            .init_resource::<Assets<WellRingsMaterial>>()
            .init_resource::<Assets<TerraceRingMaterial>>();
        app.world_mut().run_system_once(enter_room).unwrap();
        let mut query = app.world_mut().query::<&Name>();
        let names: Vec<_> = query.iter(app.world()).map(Name::as_str).collect();
        assert_eq!(names.iter().filter(|n| n.starts_with("Score facet ")).count(), 8);
        assert!(names.iter().any(|n| n.starts_with("SwitchboardLamp")));
        for prefix in ["Wall stations", "WallPanel", "WallMullion", "BearingMarker", "BearingPlinth", "BearingCap", "StationPlate"] {
            assert!(!names.iter().any(|name| name.starts_with(prefix)), "obsolete room entity {prefix}");
        }
    }

    // -- exit_room lifecycle (unconditional teardown) --

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
        // `zoomed` is a resource field, and `Screen::Room` is the only state
        // this scene graph ever occupies — every way out runs THIS exit, so
        // there is no per-target branch for it to skip.
        let mut app = zoom_lifecycle_app();
        app.world_mut().spawn(RoomRoot);
        let cam = app.world_mut().spawn((Camera::default(), RoomCamera)).id();
        app.world_mut().resource_mut::<RoomState>().zoomed = Some(Station::TimeWell);

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
    fn floor_trace_preserves_theme_brightness_below_the_crest_limit() {
        assert_eq!(floor_trace_crest([0.25, 0.125, 0.0], 0.5, 1.0), Vec4::new(0.125, 0.0625, 0.0, 0.0));
        assert_eq!(floor_trace_crest([1.0, 0.5, 0.0], 2.0, 1.0), Vec4::new(1.0, 0.5, 0.0, 0.0));
        assert_eq!(floor_trace_crest([0.0; 3], 1.0, 1.0), Vec4::ZERO);
    }

    // ── circuit-board routes (real production config) ──

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
                let crest = floor_trace_crest(route.hue, route.brightness_scale, palette.crest);
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
    fn dome_mesh_carries_a_vertex_colour_gradient() {
        let mesh = dome_mesh(100.0);
        assert!(
            mesh.attribute(Mesh::ATTRIBUTE_COLOR).is_some(),
            "the vault gradient rides on vertex colours"
        );
    }

}
