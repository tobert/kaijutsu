//! Time-well 3D scene: camera, root, screen toggle, billboarding, and card
//! motion. Owns everything that is *not* the keyed-join sync (which lives in
//! [`super::sync`]) and not the pure model (which lives in [`super::card`]).

use std::collections::HashMap;

use bevy::prelude::*;
use kaijutsu_types::ContextId;
use kaijutsu_viz::layout::CompactingBandLayout;

use super::card::{CardData, WellGeometry};
use crate::ui::screen::Screen;

// ============================================================================
// COMPONENTS
// ============================================================================

/// A context card entity in the well. Carries the stable context id (the join
/// key), the derived [`CardData`] (re-written on the layout tick), and the live
/// execution status (set on the data tick from block events).
#[derive(Component)]
pub struct Card {
    pub context_id: ContextId,
    pub data: CardData,
    /// Live status from block events; `None` until a status event arrives for
    /// this context. The data tick mutates this without ever relaying out.
    pub status: Option<kaijutsu_types::Status>,
}

/// Parent entity owning all card entities + the well camera. Despawned (with its
/// descendants) on exit so the well leaves no residue.
#[derive(Component)]
pub struct TimeWellRoot;

/// The well's 3D camera.
#[derive(Component)]
pub struct TimeWellCamera;

/// Where a card wants to be. A smoothing system eases `Transform.translation`
/// toward this each frame — the "transitions are Bevy's job" stance from the
/// design doc (no transition system, just a tween on `Transform`).
#[derive(Component)]
pub struct CardTarget(pub Vec3);

// ============================================================================
// RESOURCE
// ============================================================================

/// Live well state that survives across frames: the keyed join, the id→entity
/// map, the layout engine, the 3D geometry, and the shared mesh / per-accent
/// material handles built on first enter.
#[derive(Resource)]
pub struct TimeWellState {
    pub join: kaijutsu_viz::join::Join<ContextId, kaijutsu_client::ContextInfo>,
    pub entities: HashMap<ContextId, Entity>,
    pub layout: CompactingBandLayout,
    pub geom: WellGeometry,
    /// Shared quad mesh for every card (built lazily on first enter).
    pub card_mesh: Option<Handle<Mesh>>,
}

impl Default for TimeWellState {
    fn default() -> Self {
        use kaijutsu_viz::layout::{BandAngleConfig, LayoutConfig};
        // Fixed pitch (append-stable — NOT count-relative), but smaller than the
        // crate default's TAU/12 so a realistically-full band (~up to 24) spreads
        // around the ring without slots wrapping onto each other. Coincident cards
        // would z-fight / swap under transparent sorting; keeping pitch * count
        // < TAU avoids that for the expected card counts.
        let pitch = std::f64::consts::TAU / 24.0;
        let config = LayoutConfig {
            total_radius: 300.0,
            band_angles: [
                BandAngleConfig {
                    start_angle: 0.0,
                    pitch,
                },
                BandAngleConfig {
                    start_angle: 0.0,
                    pitch,
                },
                BandAngleConfig {
                    start_angle: 0.0,
                    pitch,
                },
            ],
        };
        Self {
            join: kaijutsu_viz::join::Join::new(),
            entities: HashMap::new(),
            layout: CompactingBandLayout::new(config),
            geom: WellGeometry::default(),
            card_mesh: None,
        }
    }
}

/// Logical card size in well units (the quad geometry).
pub const CARD_WIDTH: f32 = 64.0;
pub const CARD_HEIGHT: f32 = 40.0;

/// Card texture size (logical px the vello scene is built in, then rasterized).
/// 4× the quad units, same 1.6 aspect, so text stays crisp when sampled.
pub const CARD_TEX_W: f32 = 256.0;
pub const CARD_TEX_H: f32 = 160.0;

/// Exponential-smoothing rate for card motion (higher = snappier).
const CARD_EASE_RATE: f32 = 8.0;

// ============================================================================
// ENTER / EXIT
// ============================================================================

/// Background clear color for the well (kept opaque so the 3D camera fully
/// paints the viewport before the UI composites on top).
const WELL_BG: Color = Color::srgb(0.04, 0.05, 0.07);

/// Build the well: spawn the 3D camera + root, and re-order the existing 2D UI
/// camera to composite on top of the 3D render (transparent clear) so the dock
/// hint bar stays visible while the well owns the background. The shared card
/// mesh is built once.
pub fn enter_time_well(
    mut commands: Commands,
    mut state: ResMut<TimeWellState>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut ui_cameras: Query<&mut Camera, With<Camera2d>>,
) {
    // Build the shared card quad once.
    if state.card_mesh.is_none() {
        state.card_mesh = Some(meshes.add(Rectangle::new(CARD_WIDTH, CARD_HEIGHT)));
    }

    // The 3D well camera renders the background (order 0); the UI camera moves
    // above it (order 1) and stops clearing so the dock chrome composites over
    // the well instead of painting an opaque background over it.
    let mut n_ui = 0;
    for mut cam in ui_cameras.iter_mut() {
        cam.order = 1;
        cam.clear_color = ClearColorConfig::None;
        n_ui += 1;
    }

    commands.spawn((
        TimeWellRoot,
        Transform::default(),
        Visibility::Inherited,
        Name::new("TimeWellRoot"),
    ));

    // Camera looking down the well's depth axis. The well lives in z ∈ [-400, 0];
    // sit back far enough to frame the hot rim (radius ≈ 250) and the receding
    // colder bands.
    commands.spawn((
        TimeWellCamera,
        Camera3d::default(),
        Camera {
            order: 0,
            clear_color: ClearColorConfig::Custom(WELL_BG),
            ..default()
        },
        Transform::from_xyz(0.0, 0.0, 700.0).looking_at(Vec3::new(0.0, 0.0, -200.0), Vec3::Y),
        Name::new("TimeWellCamera"),
    ));

    info!("time-well: entered ({n_ui} ui camera(s) set to overlay)");
}

/// Tear the well down: despawn the camera + all cards, clear the id→entity map
/// and join state, and re-enable the 2D UI camera.
pub fn exit_time_well(
    mut commands: Commands,
    mut state: ResMut<TimeWellState>,
    theme: Res<crate::ui::theme::Theme>,
    roots: Query<Entity, With<TimeWellRoot>>,
    cameras: Query<Entity, With<TimeWellCamera>>,
    cards: Query<Entity, With<Card>>,
    mut ui_cameras: Query<&mut Camera, With<Camera2d>>,
) {
    for e in roots.iter().chain(cameras.iter()).chain(cards.iter()) {
        commands.entity(e).despawn();
    }

    state.entities.clear();
    // Reset the join so re-entering rebuilds from scratch (the contexts are
    // re-polled by DriftState; nothing durable is lost).
    state.join = kaijutsu_viz::join::Join::new();

    // Restore the UI camera to its standalone configuration (order 0, opaque
    // theme background) now that the well camera is gone.
    for mut cam in ui_cameras.iter_mut() {
        cam.order = 0;
        cam.clear_color = ClearColorConfig::Custom(theme.bg);
    }

    info!("time-well: exited");
}

// ============================================================================
// TOGGLE
// ============================================================================

/// Toggle into the well with Ctrl+W (when not typing); leave with Esc.
pub fn toggle_time_well(
    keys: Res<ButtonInput<KeyCode>>,
    focus_area: Res<crate::input::focus::FocusArea>,
    screen: Res<State<Screen>>,
    mut next: ResMut<NextState<Screen>>,
) {
    match screen.get() {
        Screen::Conversation => {
            if focus_area.is_text_input() {
                return;
            }
            let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
            if ctrl && keys.just_pressed(KeyCode::KeyW) {
                next.set(Screen::TimeWell);
            }
        }
        Screen::TimeWell => {
            if keys.just_pressed(KeyCode::Escape) {
                next.set(Screen::Conversation);
            }
        }
    }
}

// ============================================================================
// PER-FRAME SYSTEMS
// ============================================================================

/// Billboard every card to face the well camera. No built-in billboard in 0.18;
/// this is the one-line `looking_at` per card the design doc calls for.
pub fn billboard_cards(
    camera: Query<&GlobalTransform, With<TimeWellCamera>>,
    mut cards: Query<&mut Transform, With<Card>>,
) {
    let Ok(cam) = camera.single() else {
        return;
    };
    let cam_pos = cam.translation();
    for mut tf in cards.iter_mut() {
        // Orient the quad's visible (+Z) face toward the camera, keeping world-up
        // so text stays upright. `looking_at` points -Z at its target, so aim it
        // at the point opposite the camera (the quad mirror of the camera ray).
        let away = tf.translation * 2.0 - cam_pos;
        tf.rotation = Transform::from_translation(tf.translation)
            .looking_at(away, Vec3::Y)
            .rotation;
    }
}

/// Ease each card toward its [`CardTarget`] (exponential smoothing, frame-rate
/// independent). This is the whole "transition system" — Bevy's frame loop does
/// the work the DOM made D3 reimplement.
pub fn move_cards_toward_target(time: Res<Time>, mut cards: Query<(&mut Transform, &CardTarget)>) {
    let alpha = 1.0 - (-CARD_EASE_RATE * time.delta_secs()).exp();
    for (mut tf, target) in cards.iter_mut() {
        tf.translation = tf.translation.lerp(target.0, alpha);
    }
}

// ============================================================================
// HELPERS
// ============================================================================

/// Deterministic accent color from an accent bucket string.
///
/// Placeholder until the theme grows a context-type palette; hashes the bucket
/// to a hue so distinct context types read as distinct colors and the same type
/// is stable across frames.
pub fn accent_color(accent: &str) -> Color {
    // FNV-1a over the bytes → hue. Stable, dependency-free.
    let mut h: u32 = 2166136261;
    for b in accent.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    let hue = (h % 360) as f32;
    Color::hsl(hue, 0.55, 0.55)
}
