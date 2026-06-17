//! Time-well 3D scene: camera, root, screen toggle, billboarding, and card
//! motion. Owns everything that is *not* the keyed-join sync (which lives in
//! [`super::sync`]) and not the pure model (which lives in [`super::card`]).

use std::collections::HashMap;

use bevy::prelude::*;
use kaijutsu_types::ContextId;
use kaijutsu_viz::layout::CompactingBandLayout;

use kaijutsu_viz::layout::Band;

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
    /// Whether this card is the current selection. Drives the in-texture
    /// selection ring (see `text::build_card_scene`); flipped by
    /// [`highlight_selection`] only when it actually changes, so a card's scene
    /// rebuilds on select/deselect but not every frame.
    pub selected: bool,
    /// Whether this card is a fork-ancestor of the current selection. Drives the
    /// lineage ring (distinct from the selection ring); flipped by
    /// [`highlight_lineage`] only on change, same rebuild discipline as
    /// `selected`.
    pub in_lineage: bool,
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
    /// Band-0 (hot) context ids in slot order — what `0–9` address. Rebuilt each
    /// layout tick.
    pub hot_order: Vec<ContextId>,
    /// The currently-selected card (highlighted; the target of Enter / `c`).
    pub selected: Option<ContextId>,
    /// Per-context semantic-cluster assignment (id + kernel label), refreshed by
    /// the band-2 `get_clusters` poll. Drives the haystack's cluster-grouped
    /// angle and the cluster label on haystack cards. Empty when the kernel has
    /// no semantic index — band-2 then falls back to creation-id order.
    pub cluster_of: HashMap<ContextId, super::card::ClusterAssignment>,
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
        // Band 1 (RecentConcluded) anchors at the top (12 o'clock, +Y) so the
        // "clock of what I just finished" reads newest-up; older conclusions sweep
        // counter-clockwise from there. Hot/Haystack start at 0 (3 o'clock). The
        // band-1 order_key is conclusion-recency (see `card::layout_positions`), so
        // slot 0 == anchor == most-recently-concluded.
        let band1_anchor = std::f64::consts::FRAC_PI_2;
        let config = LayoutConfig {
            // Index order is [Haystack, RecentConcluded, Hot] (Band::index).
            total_radius: 300.0,
            band_angles: [
                BandAngleConfig {
                    start_angle: 0.0,
                    pitch,
                },
                BandAngleConfig {
                    start_angle: band1_anchor,
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
            hot_order: Vec::new(),
            selected: None,
            cluster_of: HashMap::new(),
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

/// Digit-to-slot mapping for band-0 addressing (`0–9`).
const DIGIT_KEYS: [(KeyCode, usize); 10] = [
    (KeyCode::Digit0, 0),
    (KeyCode::Digit1, 1),
    (KeyCode::Digit2, 2),
    (KeyCode::Digit3, 3),
    (KeyCode::Digit4, 4),
    (KeyCode::Digit5, 5),
    (KeyCode::Digit6, 6),
    (KeyCode::Digit7, 7),
    (KeyCode::Digit8, 8),
    (KeyCode::Digit9, 9),
];

/// Band-0 keyboard interaction (the active view): `0–9` select + switch to the
/// hot card at that slot; arrows/Tab move the selection; Enter switches to the
/// selected; `c` concludes the selected. Esc (back to conversation) lives in
/// [`toggle_time_well`].
pub fn well_keyboard(
    keys: Res<ButtonInput<KeyCode>>,
    mut state: ResMut<TimeWellState>,
    mut switch: MessageWriter<crate::view::components::ContextSwitchRequested>,
    mut next: ResMut<NextState<Screen>>,
    actor: Option<Res<crate::connection::RpcActor>>,
) {
    // `0–9`: jump straight to that hot slot and drop back into the conversation.
    for (kc, n) in DIGIT_KEYS {
        if keys.just_pressed(kc)
            && let Some(&id) = state.hot_order.get(n)
        {
            switch.write(crate::view::components::ContextSwitchRequested { context_id: id });
            next.set(Screen::Conversation);
            return;
        }
    }

    // Arrows / Tab move the selection around the hot ring.
    let dir = if keys.just_pressed(KeyCode::ArrowDown)
        || keys.just_pressed(KeyCode::ArrowRight)
        || keys.just_pressed(KeyCode::Tab)
    {
        1i32
    } else if keys.just_pressed(KeyCode::ArrowUp) || keys.just_pressed(KeyCode::ArrowLeft) {
        -1
    } else {
        0
    };
    if dir != 0 && !state.hot_order.is_empty() {
        let len = state.hot_order.len() as i32;
        let cur = state
            .selected
            .and_then(|s| state.hot_order.iter().position(|&x| x == s))
            .unwrap_or(0) as i32;
        let idx = (((cur + dir) % len) + len) % len;
        state.selected = Some(state.hot_order[idx as usize]);
    }

    // Enter: switch to the selected card.
    if keys.just_pressed(KeyCode::Enter)
        && let Some(id) = state.selected
    {
        switch.write(crate::view::components::ContextSwitchRequested { context_id: id });
        next.set(Screen::Conversation);
        return;
    }

    // `c`: conclude the selected context (fire-and-forget over RPC; the next
    // DriftState poll re-bands its card from the hot rim to the recent ring).
    if keys.just_pressed(KeyCode::KeyC)
        && let Some(id) = state.selected
        && let Some(actor) = actor
    {
        let handle = actor.handle.clone();
        bevy::tasks::IoTaskPool::get()
            .spawn(async move {
                if let Err(e) = handle.conclude(id).await {
                    log::warn!("well: conclude {} failed: {e}", id.short());
                }
            })
            .detach();
        info!("well: conclude {}", id.short());
    }
}

// ============================================================================
// PER-FRAME SYSTEMS
// ============================================================================

/// Per-band base card scale. Colder bands render smaller — the design's "history
/// grows denser, not bigger": at the tighter inner radii, full-size cards would
/// overlap into an unreadable pile, so each band shrinks to roughly fit its ring
/// pitch. (This is the interim before the step-7 chip/sediment LOD; it reads as
/// depth/coldness in the meantime.)
fn band_base_scale(band: Band) -> f32 {
    match band {
        Band::Hot => 1.0,
        Band::RecentConcluded => 0.62,
        Band::Haystack => 0.42,
    }
}

/// Scale each card toward its per-band base size, popping the selected one. The
/// `selected` flag is written so its texture grows a selection ring; it is only
/// written when it flips, so the ring rebuilds on select/deselect without
/// re-rasterizing the card every frame. The scale tween itself runs every frame
/// (eased) for a soft feel.
pub fn highlight_selection(
    state: Res<TimeWellState>,
    mut cards: Query<(&mut Card, &mut Transform)>,
) {
    for (mut card, mut tf) in cards.iter_mut() {
        let is_sel = Some(card.context_id) == state.selected;

        // Ring flag: write through the `Mut` only on a real change so we don't
        // trip `Changed<Card>` (the scene-rebuild trigger) every frame.
        if card.selected != is_sel {
            card.selected = is_sel;
        }

        // Target = the band's base size, popped by 1.35× while selected.
        let base = band_base_scale(card.data.band);
        let target = if is_sel { base * 1.35 } else { base };
        let s = tf.scale.x;
        let eased = s + (target - s) * 0.25;
        tf.scale = Vec3::splat(eased);
    }
}

/// On-demand lineage overlay: light up the fork-ancestry of the selection.
///
/// Walks the selected context's `forked_from` chain (via the join's
/// `ContextInfo`) and flags each ancestor card's `in_lineage`. Like
/// [`highlight_selection`], the flag is written only when it flips, so the
/// lineage ring rebuilds on select-change without re-rasterizing every frame.
/// Nothing selected → no lineage.
pub fn highlight_lineage(state: Res<TimeWellState>, mut cards: Query<&mut Card>) {
    use std::collections::HashSet;
    let lineage: HashSet<ContextId> = match state.selected {
        Some(sel) => super::card::ancestors(sel, |id| {
            state.join.get(&id).and_then(|c| c.forked_from)
        }),
        None => HashSet::new(),
    };
    for mut card in cards.iter_mut() {
        let in_lin = lineage.contains(&card.context_id);
        if card.in_lineage != in_lin {
            card.in_lineage = in_lin;
        }
    }
}

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
