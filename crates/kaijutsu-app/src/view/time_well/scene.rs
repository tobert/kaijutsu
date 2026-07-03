//! Time-well 3D scene: camera, root, screen toggle, billboarding, and card
//! motion. Owns everything that is *not* the keyed-join sync (which lives in
//! [`super::sync`]) and not the pure model (which lives in [`super::card`]).

use std::collections::HashMap;

use bevy::prelude::*;
use kaijutsu_types::ContextId;
use kaijutsu_viz::layout::Band;

use super::card::CardData;
use crate::ui::screen::Screen;
use super::panel::create_msdf_panel;

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
    /// Whether this context is an endpoint (source or target) of a staged drift.
    /// Drives the drift shimmer — an animated HDR sheen sweeping the card body
    /// (the "drift = shimmer" bling). Flipped by [`highlight_drift`] only on
    /// change, same rebuild discipline as `selected`/`in_lineage`.
    pub drifting: bool,
    /// Base render scale from this card's position on the vortex spiral (1.0 at
    /// the mouth, shrinking toward the throat — see [`super::card::spiral_scale`]).
    /// [`highlight_selection`] eases toward this, popping the selection.
    pub base_scale: f32,
}

/// Parent entity owning all card entities + the well camera. Despawned (with its
/// descendants) on exit so the well leaves no residue.
#[derive(Component)]
pub struct TimeWellRoot;

/// The well's 3D camera.
#[derive(Component)]
pub struct TimeWellCamera;

/// The center-bottom reading slot: a single large card, parented to the camera
/// (HUD-stable), that renders the current selection at readable size. Updated by
/// `text::update_reading_card` on selection change.
#[derive(Component)]
pub struct ReadingCard;

/// The well's base ring deck: a flat disc behind the cards (XY plane) that
/// renders the concentric rings + spiral core + activity ripples (the well's
/// "pulse"). Driven by [`WellRingsMaterial`] uniforms from [`tick_and_sync_rings`].
/// Despawned on exit alongside the root.
#[derive(Component)]
pub struct WellRingsDeck;

/// A magic-circle ring at one terrace boundary (the Konosuba/"Explosion"-spell
/// aesthetic — concentric glyph rings, counter-rotating, receding into the
/// funnel). One entity per interior terrace boundary (see
/// [`super::card::terrace_ring_geometry`]), driven by
/// [`crate::shaders::TerraceRingMaterial`]. Despawned on exit alongside the
/// rest of the well.
#[derive(Component)]
pub struct TerraceRing;

/// Where a card wants to be. A smoothing system eases `Transform.translation`
/// toward this each frame — the "transitions are Bevy's job" stance from the
/// design doc (no transition system, just a tween on `Transform`).
#[derive(Component)]
pub struct CardTarget(pub Vec3);

/// A card's discrete seat on its band ring: which band, its within-ring index,
/// and the ring's card count. `sync` writes this from the recency-ordered band
/// layout; [`spin_rings`] recomputes the card's [`CardTarget`] from it each
/// frame using the ring's eased rotation (the projector spin), so the seat is
/// the durable position and the world target is derived.
#[derive(Component)]
pub struct RingSeat {
    pub band: kaijutsu_viz::layout::Band,
    pub within_index: usize,
    pub ring_len: usize,
}

// ============================================================================
// RESOURCE
// ============================================================================

/// Live well state that survives across frames: the keyed join, the id→entity
/// map, the spiral order, and the shared mesh / per-accent material handles built
/// on first enter.
#[derive(Resource)]
pub struct TimeWellState {
    pub join: kaijutsu_viz::join::Join<ContextId, kaijutsu_client::ContextInfo>,
    pub entities: HashMap<ContextId, Entity>,
    /// Shared quad mesh for every card (built lazily on first enter).
    pub card_mesh: Option<Handle<Mesh>>,
    /// The whole well as one ordered spiral, **mouth → throat**: `HotNow` →
    /// `ThisWeek` → `ThirtyDays` → `Horizon`, each band in its own recency
    /// order — see [`super::card::spiral_order`] / [`super::card::spiral_positions`]
    /// — rebuilt each layout tick. The single source of nav order: a card's
    /// index here is its odometer address (Left/Right = ±1, Up/Down = ±10,
    /// digits = the first decade at the mouth); its *world position* now comes
    /// from the terraced `(band, within_index)` pair instead (Stage 1 Slice F).
    pub spiral_order: Vec<ContextId>,
    /// Each band ring's cards in within-ring (recency) order, indexed by
    /// [`kaijutsu_viz::layout::Band::index`] — the ring-centric nav's source of
    /// truth. `(focused_ring, ring_pos)` indexes into `ring_cards[focused_ring]`
    /// to resolve [`selected`](Self::selected). Rebuilt each layout tick from
    /// [`super::card::band_orders`].
    pub ring_cards: [Vec<ContextId>; super::card::N_BANDS],
    /// Which band ring is currently focused (0 = `HotNow` at the mouth …
    /// `N_BANDS-1` = `Horizon` at the throat). Up/Down change it.
    pub focused_ring: usize,
    /// Position within the focused ring (Left/Right walk it, wrapping). Carried
    /// across Up/Down (clamped to the new ring's size).
    pub ring_pos: usize,
    /// Per-ring current (eased) rotation in radians — the live projector spin,
    /// advanced toward `ring_rotation_target` by [`spin_rings`].
    pub ring_rotation: [f32; super::card::N_BANDS],
    /// Per-ring rotation goal: nav sets the focused ring's target so the selected
    /// card spins to the gate (see [`super::card::spin_target_to_gate`]).
    pub ring_rotation_target: [f32; super::card::N_BANDS],
    /// The currently-selected card (highlighted; the target of Enter / `c`).
    /// Derived from `(focused_ring, ring_pos)` on every nav change and layout tick.
    pub selected: Option<ContextId>,
    /// Whether the well is *focused* on the selection: Enter (from the overview)
    /// dollies the camera into the focus card; Esc backs out; a second Enter
    /// (while focused) commits — switches to the context. Drives the camera pose
    /// in [`ease_camera_to_selection`].
    pub focused: bool,
    /// Per-context semantic-cluster assignment (id + kernel label), refreshed by
    /// the `get_clusters` poll. Drives the cluster label on `Horizon` cards
    /// (Stage 3 will extend this to cluster-grouped angle within a band; for
    /// now it's label-only — see `card::card_from`). Empty when the kernel has
    /// no semantic index.
    pub cluster_of: HashMap<ContextId, super::card::ClusterAssignment>,
}

impl Default for TimeWellState {
    fn default() -> Self {
        Self {
            join: kaijutsu_viz::join::Join::new(),
            entities: HashMap::new(),
            card_mesh: None,
            spiral_order: Vec::new(),
            ring_cards: std::array::from_fn(|_| Vec::new()),
            focused_ring: 0,
            ring_pos: 0,
            ring_rotation: [0.0; super::card::N_BANDS],
            ring_rotation_target: [0.0; super::card::N_BANDS],
            selected: None,
            focused: false,
            cluster_of: HashMap::new(),
        }
    }
}

/// Logical card size in well units (the quad geometry). Bigger than the original
/// 64×40 so the spiral's cards read larger and closer together (1.6 aspect, to
/// match the card texture so text isn't distorted).
pub const CARD_WIDTH: f32 = 120.0;
pub const CARD_HEIGHT: f32 = 75.0;

/// Rim-card thickness (well units) along the card's local Z: rim cards are thin
/// 3D **blocks**, not flat quads, so a card facing away from the camera (far
/// arc of a ring-aligned band) shows its rear large face — which is
/// front-facing from the camera's side and so still renders under default
/// back-face culling. Kept small so the card still reads as a card, not a
/// slab. **Amy-tunable.**
pub const CARD_THICKNESS: f32 = 8.0;

/// Card texture size (logical px the vello scene is built in, then rasterized).
/// 4× the quad units, same 1.6 aspect, so text stays crisp when sampled.
pub const CARD_TEX_W: f32 = 256.0;
pub const CARD_TEX_H: f32 = 160.0;

/// Focus-card texture size (logical px the vello scene is built in). A tall card
/// aspect (1.6) — the in-world focus card is a card, not a bar. High-res so it
/// stays crisp when the camera dollies in on focus.
pub const READING_TEX_W: f32 = 512.0;
pub const READING_TEX_H: f32 = 320.0;

/// In-world focus-card quad size (well units, 1.6 aspect — much larger than a
/// rim card so it reads as the focus pedestal lower-center of the well).
pub const FOCUS_QUAD_W: f32 = 380.0;
pub const FOCUS_QUAD_H: f32 = 237.5;

/// World position of the focus card: lower-center and forward (+Z, toward the
/// camera) so it floats in front of the rings at the mouth of the well.
pub const FOCUS_CARD_POS: Vec3 = Vec3::new(0.0, -40.0, 260.0);

/// Camera distance in front of the focus card when focused (larger = card fills
/// less of the frame). Tuned a touch back so the focused card isn't oversized.
const FOCUS_DOLLY: f32 = 430.0;

/// Side length of the square ring-deck quad (world units). Comfortably larger
/// than the hot rim (`total_radius` 420) so the unit disc the shader draws fills
/// the well; the corners outside the disc are transparent.
const RING_DECK_SIZE: f32 = 1100.0;

/// Depth (along the funnel's local −Z) of the ring deck: just past the deepest
/// band ring (`Horizon` sits at ≈ −690 under the per-band `band_ring` stack) so
/// the vortex/spiral core is the **throat floor** all the rings spiral down
/// into — on the same funnel axis, below every ring. Lifted + tilted by the
/// shared [`super::card::well_tilt_quat`] so the core sits at the low, receded
/// throat and faces up toward the camera. **Amy-tunable.**
const RING_DECK_DEPTH: f32 = -850.0;

/// Terrace-ring quad side length, as a multiple of the ring radius. The quad is
/// comfortably larger than the ring (side = scale × radius) so the annulus band
/// — which the shader draws centered on the *ring* radius (= where the cards are
/// seated; see the spawn loop) — sits well inside the quad's inscribed circle
/// with room for its half-width + corner-fade. **Amy-tunable.**
const TERRACE_RING_QUAD_SCALE: f32 = 2.8;

/// Half-width (fraction of the quad half-extent) of the visible annulus band,
/// centered on the ring radius. Widened for the ornate grid (concentric
/// sub-rings + two-tier spokes + hexagram need room to read). **Amy-tunable.**
const TERRACE_RING_BAND_HALF_WIDTH: f32 = 0.09;

/// Base spin rate (radians/sec-ish, tune by eye) for terrace ring `k`; each
/// deeper ring spins a touch faster so the funnel reads as receding motion.
/// **Amy-tunable — kept calm for the first cut.**
const TERRACE_RING_SPIN_BASE: f32 = 0.10;
const TERRACE_RING_SPIN_STEP: f32 = 0.04;

/// Overall alpha/intensity for the terrace rings — kept low for the first cut
/// so the driver tunes up from a subtle starting point. **Amy-tunable.**
const TERRACE_RING_ALPHA: f32 = 0.35;

/// Glyph color for the terrace rings: an icy cyan-white that reads against
/// [`WELL_BG`] (the concept-art magic-circle palette). **Amy-tunable.**
const TERRACE_RING_COLOR: Vec3 = Vec3::new(0.55, 0.85, 1.0);

/// Per-band recline (radians) layered on the billboard. With the whole funnel now
/// reclined in space (see [`super::card::WELL_TILT`]), the cards read their 3D
/// form from their *positions* and should face the camera cleanly, so the recline
/// is off. Kept as a per-band knob in case a slight lean reads better. Tunable.
fn card_tilt(band: Band) -> f32 {
    match band {
        Band::HotNow => 0.0,
        Band::ThisWeek => 0.0,
        Band::ThirtyDays => 0.0,
        Band::Horizon => 0.0,
    }
}

/// Exponential-smoothing rate for card motion (higher = snappier).
const CARD_EASE_RATE: f32 = 8.0;

/// Exponential-smoothing rate for the per-ring projector spin (how fast a ring
/// rotates its selected card to the gate). **Amy-tunable.**
const RING_SPIN_EASE_RATE: f32 = 6.0;

/// Blend dial for rim-card orientation: `0.0` = today's full camera-billboard,
/// `1.0` = full ring-aligned (cards stand around their ring like a carousel,
/// face-normal radial-outward, up along the funnel axis). Intermediate values
/// slerp between the two. The focus [`ReadingCard`] is unaffected (always
/// billboarded). **Amy-tunable.**
const RING_ALIGN: f32 = 1.0;

/// Exponential-smoothing rate for the camera follow (lower = a slower, weightier
/// glide than the cards, so the view leans rather than snaps).
const CAMERA_EASE_RATE: f32 = 4.0;

// ============================================================================
// ENTER / EXIT
// ============================================================================

/// Background clear color for the well (kept opaque so the 3D camera fully
/// paints the viewport before the UI composites on top).
const WELL_BG: Color = Color::srgb(0.04, 0.05, 0.07);

/// Build the well: repurpose the shared app camera (mark it `TimeWellCamera`,
/// swap its clear color to the well background, and frame the rings) and spawn
/// the root + focus card. There is no second camera — the conversation UI and the
/// well's 3D meshes share the one always-on `Camera3d` (see `main::setup_camera`),
/// so the old 3D-background / 2D-overlay composite is gone. The shared card mesh
/// is built once.
pub fn enter_time_well(
    mut commands: Commands,
    mut state: ResMut<TimeWellState>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<crate::shaders::WellCardMaterial>>,
    mut ring_materials: ResMut<Assets<crate::shaders::WellRingsMaterial>>,
    mut terrace_ring_materials: ResMut<Assets<crate::shaders::TerraceRingMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut app_camera: Query<(Entity, &mut Camera, &mut Transform), With<Camera3d>>,
) {
    // Fresh entry always starts in the overview (not focused).
    state.focused = false;

    // Build the shared rim-card block once (a thin 3D box, not a flat quad, so
    // both large faces read from their own side — see [`card_block_mesh`]).
    if state.card_mesh.is_none() {
        state.card_mesh = Some(meshes.add(card_block_mesh()));
    }

    // Repurpose the one app camera for the well: mark it so the well's per-frame
    // camera systems (`ease_camera_to_selection`, `billboard_cards`) find it, swap
    // its clear color to the well background, and set the base framing — pulled
    // back and tilted up so the full hot rim (radius ≈ 420) sits in the top
    // ~two-thirds, with the colder bands receding behind it.
    if let Ok((cam_entity, mut cam, mut tf)) = app_camera.single_mut() {
        commands.entity(cam_entity).insert(TimeWellCamera);
        cam.clear_color = ClearColorConfig::Custom(WELL_BG);
        *tf = Transform::from_translation(CAM_BASE_POS).looking_at(CAM_BASE_LOOK, Vec3::Y);
    }

    commands.spawn((
        TimeWellRoot,
        Transform::default(),
        Visibility::Inherited,
        Name::new("TimeWellRoot"),
    ));

    // Base ring deck: a flat disc behind the cards that renders the well's pulse
    // (concentric rings + spiral core + activity ripples). Driven per-frame by
    // `tick_and_sync_rings`. Not billboarded — it faces the camera (+Z) as a
    // fixed floor; the shader fades its square corners to nothing.
    let deck_mesh = meshes.add(Rectangle::new(RING_DECK_SIZE, RING_DECK_SIZE));
    // Warm gold core + cyan-blue rings (the concept-art palette, mockups 27/33).
    let deck_material = ring_materials.add(crate::shaders::WellRingsMaterial::new(
        Vec4::new(1.0, 0.62, 0.20, 1.0),
        Vec4::new(0.35, 0.62, 1.0, 1.0),
    ));
    // Tilt + place the deck on the same reclined funnel axis as the cards: its
    // center rides to the throat (lifted depth) and its face tips up toward the
    // camera, so the spiral core reads as the bottom of the vortex.
    let tilt = super::card::well_tilt_quat();
    let deck_pos = tilt * Vec3::new(0.0, 0.0, RING_DECK_DEPTH);
    commands.spawn((
        WellRingsDeck,
        Mesh3d(deck_mesh),
        MeshMaterial3d(deck_material),
        Transform {
            translation: deck_pos,
            rotation: tilt,
            scale: Vec3::ONE,
        },
        Visibility::Inherited,
        Name::new("WellRingsDeck"),
    ));

    // Band magic-circle rings: one annulus quad per band ring (the
    // Konosuba/"Explosion"-spell aesthetic), counter-rotating and receding into
    // the funnel on the same tilted axis as the deck/cards. Each quad is sized
    // to ITS ring's radius, and the band is drawn centered on that radius so it
    // lands on the cards seated around the ring.
    for (k, (radius, depth)) in super::card::terrace_ring_geometry().into_iter().enumerate() {
        let side = TERRACE_RING_QUAD_SCALE * radius;
        let ring_mesh = meshes.add(Rectangle::new(side, side));
        // The shader's radial coord is 0 at center → 1 at the quad edge (half
        // -extent = side/2). Center the band on the ring radius so it sits on the
        // seated cards: center_frac = radius / half_extent (= 2 / QUAD_SCALE).
        let center_frac = radius / (side * 0.5);
        let inner_frac = center_frac - TERRACE_RING_BAND_HALF_WIDTH;
        let outer_frac = center_frac + TERRACE_RING_BAND_HALF_WIDTH;
        let spin_dir = if k % 2 == 0 { 1.0 } else { -1.0 };
        let spin_rate = TERRACE_RING_SPIN_BASE + TERRACE_RING_SPIN_STEP * k as f32;
        let ring_material = terrace_ring_materials.add(crate::shaders::TerraceRingMaterial::new(
            inner_frac,
            outer_frac,
            spin_rate,
            spin_dir,
            TERRACE_RING_COLOR,
            TERRACE_RING_ALPHA,
        ));
        let ring_pos = tilt * Vec3::new(0.0, 0.0, depth);
        commands.spawn((
            TerraceRing,
            Mesh3d(ring_mesh),
            MeshMaterial3d(ring_material),
            Transform {
                translation: ring_pos,
                rotation: tilt,
                scale: Vec3::ONE,
            },
            Visibility::Inherited,
            Name::new(format!("TerraceRing{k}")),
        ));
    }

    // Focus card: an in-world 3D card floating lower-center at the mouth of the
    // well (not a flat HUD panel — it lives in the scene, billboarded, and the
    // camera dollies into it on focus). It renders the current selection;
    // `update_reading_card` fills its texture (blank until a selection exists).
    let focus_mesh = meshes.add(Rectangle::new(FOCUS_QUAD_W, FOCUS_QUAD_H));
    let (focus_image, panel) =
        create_msdf_panel(&mut images, READING_TEX_W as u32, READING_TEX_H as u32);
    let focus_material = materials.add(crate::shaders::WellCardMaterial {
        texture: focus_image,
        accent: Vec4::ZERO, // filled by update_reading_card on the first selection
        params: Vec4::ZERO,
        shape: card_shape(),
        border: Vec4::ZERO,
    });
    commands.spawn((
        ReadingCard,
        Mesh3d(focus_mesh),
        MeshMaterial3d(focus_material),
        Transform::from_translation(FOCUS_CARD_POS),
        Visibility::Inherited,
        // MSDF owns this texture (clears + renders text on transparent); the
        // shader draws the body. No vello — pure MSDF, no UiVectorScene.
        panel,
        Name::new("ReadingCard"),
    ));

    info!("time-well: entered (shared app camera repurposed for the well)");
}

/// Tear the well down: despawn the root + all cards, clear the id→entity map and
/// join state, and hand the shared app camera back to the conversation (drop the
/// well marker, restore the theme background clear). The camera itself is *not*
/// despawned — it is the app's one always-on camera.
pub fn exit_time_well(
    mut commands: Commands,
    mut state: ResMut<TimeWellState>,
    theme: Res<crate::ui::theme::Theme>,
    roots: Query<Entity, With<TimeWellRoot>>,
    cards: Query<Entity, With<Card>>,
    reading: Query<Entity, With<ReadingCard>>,
    decks: Query<Entity, With<WellRingsDeck>>,
    terrace_rings: Query<Entity, With<TerraceRing>>,
    mut app_camera: Query<(Entity, &mut Camera), With<TimeWellCamera>>,
) {
    for e in roots
        .iter()
        .chain(cards.iter())
        .chain(reading.iter())
        .chain(decks.iter())
        .chain(terrace_rings.iter())
    {
        commands.entity(e).despawn();
    }

    state.entities.clear();
    state.focused = false;
    // Reset ring-centric nav so re-entering starts at the mouth ring, gate-front.
    state.ring_cards = std::array::from_fn(|_| Vec::new());
    state.focused_ring = 0;
    state.ring_pos = 0;
    state.ring_rotation = [0.0; super::card::N_BANDS];
    state.ring_rotation_target = [0.0; super::card::N_BANDS];
    // Reset the join so re-entering rebuilds from scratch (the contexts are
    // re-polled by DriftState; nothing durable is lost).
    state.join = kaijutsu_viz::join::Join::new();

    // Hand the shared camera back to the conversation: drop the well marker (so
    // the well's camera systems stop driving it) and restore the theme clear.
    if let Ok((cam_entity, mut cam)) = app_camera.single_mut() {
        commands.entity(cam_entity).remove::<TimeWellCamera>();
        cam.clear_color = ClearColorConfig::Custom(theme.bg);
    }

    info!("time-well: exited");
}

// ============================================================================
// TOGGLE
// ============================================================================

/// Enter the well with Ctrl+W (when not typing). Leaving is Esc, handled in
/// [`well_keyboard`] so it can be focus-aware (Esc backs out of focus first,
/// then leaves the well).
pub fn toggle_time_well(
    keys: Res<ButtonInput<KeyCode>>,
    focus_area: Res<crate::input::focus::FocusArea>,
    screen: Res<State<Screen>>,
    mut next: ResMut<NextState<Screen>>,
) {
    if *screen.get() == Screen::Conversation {
        if focus_area.is_text_input() {
            return;
        }
        let ctrl = keys.pressed(KeyCode::ControlLeft) || keys.pressed(KeyCode::ControlRight);
        if ctrl && keys.just_pressed(KeyCode::KeyW) {
            next.set(Screen::TimeWell);
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

/// Time-well keyboard navigation: **ring-centric**, with a Kodak-projector spin.
/// Selection is `(focused_ring, ring_pos)`; the card at that seat is
/// [`TimeWellState::selected`], so HUD / lineage / highlight / Enter all follow.
/// - `0–9` — quick-jump to that flat spiral index near the mouth: select + exit.
/// - **Left / Right / Tab** — step the position within the focused ring
///   (wrapping), spinning the ring so the selected card eases to the front gate.
/// - **Up / Down** — change the focused ring (Up → shallower/mouth, Down →
///   deeper/throat), carrying the position index (clamped); the newly focused
///   ring spins its selected card to the gate and the camera retargets to it.
/// - **Enter** focuses then commits; **`c`** concludes the selection.
///
/// Esc (focus-aware, back to conversation) is handled below.
pub fn well_keyboard(
    keys: Res<ButtonInput<KeyCode>>,
    mut state: ResMut<TimeWellState>,
    mut switch: MessageWriter<crate::view::components::ContextSwitchRequested>,
    mut next: ResMut<NextState<Screen>>,
    actor: Option<Res<crate::connection::RpcActor>>,
) {
    // `0–9`: jump straight to that mouth-end index and drop into the conversation.
    for (kc, n) in DIGIT_KEYS {
        if keys.just_pressed(kc)
            && let Some(&id) = state.spiral_order.get(n)
        {
            switch.write(crate::view::components::ContextSwitchRequested { context_id: id });
            next.set(Screen::Conversation);
            return;
        }
    }

    let mut nav_changed = false;

    // Left/Right (Tab = Right): walk the focused ring, wrapping, and spin it so
    // the newly selected seat rolls to the gate.
    let lr = if keys.just_pressed(KeyCode::ArrowRight) || keys.just_pressed(KeyCode::Tab) {
        1
    } else if keys.just_pressed(KeyCode::ArrowLeft) {
        -1
    } else {
        0
    };
    if lr != 0 {
        let fr = state.focused_ring;
        let len = state.ring_cards.get(fr).map(|v| v.len()).unwrap_or(0);
        if len > 0 {
            let pos = super::card::step_ring_pos(state.ring_pos, len, lr);
            state.ring_pos = pos;
            let cur = state.ring_rotation_target[fr];
            state.ring_rotation_target[fr] = super::card::spin_target_to_gate(cur, pos, len);
            nav_changed = true;
        }
    }

    // Up/Down: change the focused ring (clamp 0..N_BANDS-1), carry the position
    // onto the new ring, spin the new ring to the gate. The camera follows
    // because `ease_camera_to_focused_ring` keys on `focused_ring`.
    let ud = if keys.just_pressed(KeyCode::ArrowUp) {
        -1
    } else if keys.just_pressed(KeyCode::ArrowDown) {
        1
    } else {
        0
    };
    if ud != 0 {
        let fr = state.focused_ring as i32;
        let new_ring = (fr + ud).clamp(0, super::card::N_BANDS as i32 - 1) as usize;
        if new_ring != state.focused_ring {
            let new_len = state.ring_cards.get(new_ring).map(|v| v.len()).unwrap_or(0);
            let pos = super::card::carry_ring_pos(state.ring_pos, new_len);
            state.focused_ring = new_ring;
            state.ring_pos = pos;
            let cur = state.ring_rotation_target[new_ring];
            state.ring_rotation_target[new_ring] =
                super::card::spin_target_to_gate(cur, pos, new_len.max(1));
            nav_changed = true;
        }
    }

    // After any nav change, re-derive the selection from the seat so every
    // downstream system (highlight, HUD, lineage) follows.
    if nav_changed {
        let fr = state.focused_ring;
        let pos = state.ring_pos;
        let sel = state.ring_cards.get(fr).and_then(|v| v.get(pos)).copied();
        state.selected = sel;
    }

    // Enter is two-stage: from the overview it *focuses* (the camera dollies into
    // the focus card); a second Enter while focused *commits* — switches to the
    // context and leaves the well.
    if keys.just_pressed(KeyCode::Enter) {
        if state.focused {
            if let Some(id) = state.selected {
                switch.write(crate::view::components::ContextSwitchRequested { context_id: id });
                next.set(Screen::Conversation);
            }
        } else if state.selected.is_some() {
            state.focused = true;
        }
        return;
    }

    // Esc backs out: from focus it returns to the overview; from the overview it
    // leaves the well. (This is why Esc lives here, not in `toggle_time_well`.)
    if keys.just_pressed(KeyCode::Escape) {
        if state.focused {
            state.focused = false;
        } else {
            next.set(Screen::Conversation);
        }
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

/// Scale each card toward its spiral base size (set in `sync` from
/// [`super::card::spiral_scale`] — full at the mouth, shrinking toward the
/// throat), popping the selected one. The `selected` flag is written only when it
/// flips so the selection ring rebuilds on select/deselect without re-rasterizing
/// every frame; the scale tween itself runs every frame (eased) for a soft feel.
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

        // Target = the card's spiral base size, popped by 1.35× while selected.
        let base = card.base_scale;
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

/// Drift shimmer overlay: flag every card whose context is an endpoint (source or
/// target) of a staged drift, so the shader sweeps an animated HDR sheen across it
/// (the "drift = shimmer" bling). Reads the staged queue off the shared
/// [`DriftState`] poll — no extra wire. Like [`highlight_lineage`], the flag is
/// written only when it flips, so the shimmer turns on/off without re-rasterizing
/// every card every poll. Empty staged queue → nothing shimmers.
pub fn highlight_drift(drift: Res<crate::ui::drift::DriftState>, mut cards: Query<&mut Card>) {
    let drifting = super::card::drift_endpoints(&drift.staged);
    for mut card in cards.iter_mut() {
        let is_drifting = drifting.contains(&card.context_id);
        if card.drifting != is_drifting {
            card.drifting = is_drifting;
        }
    }
}

/// Show the in-world focus card only when *focused* (Enter-to-focus / the dive).
/// In the overview it stays hidden so the well's mouth — the glowing core +
/// activity rings — is the open browser space and the selected card's detail
/// reads off the edge HUD instead (see [`super::hud`]). Also sidesteps the
/// blank-white empty-selection state, which only the focus card ever showed.
pub fn sync_focus_card_visibility(
    state: Res<TimeWellState>,
    mut card: Query<&mut Visibility, With<ReadingCard>>,
) {
    let want = if state.focused {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    if let Ok(mut vis) = card.single_mut()
        && *vis != want
    {
        *vis = want;
    }
}

/// Ingest the kernel-wide event stream into the well's pulse. Every block event
/// (token streaming weighs most — see [`super::activity::event_signal`]) raises
/// the global energy and fires a **ripple at the producing context's ring
/// angle** (`atan2(card.y, card.x)`), so a busy conversation throws a wavefront
/// out from its direction on the deck. Events for contexts not currently shown
/// still raise the global energy (ripple fired at angle 0).
pub fn accumulate_ring_activity(
    mut events: MessageReader<crate::connection::ServerEventMessage>,
    mut activity: ResMut<super::activity::RingActivity>,
    cards: Query<(&Card, &CardTarget)>,
) {
    for crate::connection::ServerEventMessage(ev) in events.read() {
        if let Some((ctx, weight)) = super::activity::event_signal(ev) {
            let angle = cards
                .iter()
                .find(|(c, _)| c.context_id == ctx)
                .map(|(_, t)| t.0.y.atan2(t.0.x))
                .unwrap_or(0.0);
            activity.record(ctx, angle, weight);
        }
    }
}

/// Advance the well's pulse and push it into the deck material uniforms: the
/// global `energy` (ring brightness / flow / core spin) and the packed ripple
/// array (`[cos, sin, age_norm, intensity]`, unused slots `intensity = 0`).
pub fn tick_and_sync_rings(
    time: Res<Time>,
    mut activity: ResMut<super::activity::RingActivity>,
    mut ring_materials: ResMut<Assets<crate::shaders::WellRingsMaterial>>,
    deck: Query<&MeshMaterial3d<crate::shaders::WellRingsMaterial>, With<WellRingsDeck>>,
) {
    activity.tick(time.delta_secs());

    let Ok(handle) = deck.single() else {
        return;
    };
    let Some(mat) = ring_materials.get_mut(&handle.0) else {
        return;
    };

    mat.energy.x = activity.energy;

    let mut packed = [Vec4::ZERO; super::activity::MAX_RIPPLES];
    for (i, r) in activity
        .ripples
        .iter()
        .take(super::activity::MAX_RIPPLES)
        .enumerate()
    {
        let age_norm = (r.age / super::activity::RIPPLE_LIFETIME).clamp(0.0, 1.0);
        packed[i] = Vec4::new(r.angle.cos(), r.angle.sin(), age_norm, r.intensity);
    }
    mat.ripples = packed;
}

/// Base camera framing. The well's recline now lives in the *geometry* (the
/// funnel is tipped back about X, see [`super::card::WELL_TILT`]), so the camera
/// only needs a gentle downward look *into* the flared mouth. Aiming a little
/// above the origin drops the low throat toward bottom-center of the frame, with
/// the mouth opening up toward the camera (the concept well, mockups 27/31). Both
/// the recline angle and this pose are the two knobs we tune together.
const CAM_BASE_POS: Vec3 = Vec3::new(0.0, 240.0, 900.0);
const CAM_BASE_LOOK: Vec3 = Vec3::new(0.0, 40.0, -150.0);

/// Camera framing of the focused ring (first cut — Amy live-tunes on the runner).
/// Back-off distance along the funnel axis, **as a multiple of the ring radius**,
/// so a bigger ring is framed from proportionally further back (neighbor rings
/// bleed off the top/bottom edges).
const RING_CAM_BACK: f32 = 1.8;
/// World-Y lift of the camera. With the gate-normal framing the gate card's face
/// points down-and-forward, so the normal back-off pulls the camera below the
/// gate; this lift raises it back to roughly level / gently looking down. Higher
/// = steeper look-down onto the ring. Amy-tunable.
const RING_CAM_LIFT: f32 = 450.0;
/// How far in front of the ring center (along the axis, × radius) the look-point
/// leads — 0 looks straight at the ring plane.
const RING_CAM_LOOK_LEAD: f32 = 0.0;

/// Ease the well camera, exponentially smoothed (slower than the cards, so the
/// view glides):
/// - **focused** (Enter-to-focus) → dolly straight in front of the focus card so
///   it fills the view; Esc backs out.
/// - **overview** → frame the **focused ring** ([`TimeWellState::focused_ring`]):
///   look at its center (its tilted depth plane), backed off along the funnel
///   axis by [`RING_CAM_BACK`]×radius and lifted, so that ring roughly fills the
///   view with the neighbor rings bleeding off. Up/Down retarget it.
///
/// The full dive *through* the card into the conversation is the later JOIN
/// transition; here Enter-focus only zooms to the pedestal.
pub fn ease_camera_to_focused_ring(
    time: Res<Time>,
    state: Res<TimeWellState>,
    mut cam: Query<&mut Transform, With<TimeWellCamera>>,
) {
    let Ok(mut tf) = cam.single_mut() else {
        return;
    };

    let (desired_pos, desired_look) = if state.focused {
        // Dolly to a head-on framing of the focus card so it dominates the view.
        (FOCUS_CARD_POS + Vec3::new(0.0, 0.0, FOCUS_DOLLY), FOCUS_CARD_POS)
    } else {
        // Frame the focused ring. Its center rides the tilted funnel axis at the
        // ring's depth; the axis (tilt·+Z) points up-and-toward the camera.
        let band = kaijutsu_viz::layout::ALL_BANDS
            [state.focused_ring.min(super::card::N_BANDS - 1)];
        let (radius, depth) = super::card::band_ring(band);
        let tilt = super::card::well_tilt_quat();
        // Frame the GATE card face-on: sit out along the outward face-normal of the
        // seat the selected slide spins to (`card::GATE_ANGLE`), backed off ∝ radius
        // and lifted, looking at the gate point. Whatever card is at the gate reads
        // face-on and roughly centered; the ring curves away behind it (relief), and
        // the shallower/deeper rings sit above/below by depth and bleed off the edges.
        let a = super::card::GATE_ANGLE;
        let gate = tilt * Vec3::new(radius * a.cos(), radius * a.sin(), depth);
        let normal = tilt * Vec3::new(-a.sin(), a.cos(), 0.0); // gate slide's face normal
        let pos = gate + normal * (radius * RING_CAM_BACK) + Vec3::Y * RING_CAM_LIFT;
        let look = gate + Vec3::Y * RING_CAM_LOOK_LEAD;
        (pos, look)
    };

    let desired = Transform::from_translation(desired_pos).looking_at(desired_look, Vec3::Y);
    let alpha = 1.0 - (-CAMERA_EASE_RATE * time.delta_secs()).exp();
    tf.translation = tf.translation.lerp(desired.translation, alpha);
    tf.rotation = tf.rotation.slerp(desired.rotation, alpha);
}

/// Billboard every card to face the well camera. No built-in billboard in 0.18;
/// this is the one-line `looking_at` per card the design doc calls for.
pub fn billboard_cards(
    camera: Query<&GlobalTransform, With<TimeWellCamera>>,
    mut cards: Query<(&mut Transform, Option<&Card>), Or<(With<Card>, With<ReadingCard>)>>,
) {
    let Ok(cam) = camera.single() else {
        return;
    };
    let cam_pos = cam.translation();
    for (mut tf, card) in cards.iter_mut() {
        // Orient the quad's visible (+Z) face toward the camera, keeping world-up
        // so text stays upright. `looking_at` points -Z at its target, so aim it
        // at the point opposite the camera (the quad mirror of the camera ray).
        let away = tf.translation * 2.0 - cam_pos;
        let billboard_rot = Transform::from_translation(tf.translation)
            .looking_at(away, Vec3::Y)
            .rotation;

        // The focus card (no `Card`) stays fully billboarded, head-on.
        let Some(card) = card else {
            tf.rotation = billboard_rot;
            continue;
        };

        // Rim cards: ring-align them so they stand around their ring like a
        // carousel — face-normal radial-outward, up along the funnel axis —
        // blended against the billboard by `RING_ALIGN`.
        let tilt = super::card::well_tilt_quat();
        let local = tilt.inverse() * tf.translation; // back into funnel-local space
        let radial_local = Vec3::new(local.x, local.y, 0.0).normalize_or_zero();
        if radial_local == Vec3::ZERO {
            // Card sits on the axis: no radial direction to face — billboard it.
            tf.rotation = billboard_rot;
            continue;
        }
        let axis = tilt * Vec3::Z; // world-space ring/funnel axis = card up
        // Slide-tray orientation: the card stands PERPENDICULAR to the ring arc —
        // like a slide in a Kodak Carousel cartridge or a tooth on a gear. Its
        // broad face points along the ring TANGENT (toward its neighbours), its
        // width runs radially (the card sticks out like a spoke), its height along
        // the funnel axis. (Radial-facing = face-tangent-to-arc was the previous
        // look Amy rejected — that made a smooth cylinder wall, not standing slides.)
        let tangent_local = Vec3::new(-radial_local.y, radial_local.x, 0.0);
        let tangent = tilt * tangent_local; // world-space ring tangent
        // `looking_to` points -Z at its direction, so aim -tangent to put the
        // visible +Z face along the tangent, with the funnel axis as up.
        let ring_rot = Transform::from_translation(tf.translation)
            .looking_to(-tangent, axis)
            .rotation;
        let mut rot = billboard_rot.slerp(ring_rot, RING_ALIGN);
        // Per-band `card_tilt` recline only for the billboard share — the
        // axis-up ring orientation supersedes it, so fade it out as RING_ALIGN
        // rises (no double-reclining).
        rot *= Quat::from_rotation_x(-card_tilt(card.data.band) * (1.0 - RING_ALIGN));
        tf.rotation = rot;
    }
}

/// Projector spin: ease each ring's `ring_rotation` toward its
/// `ring_rotation_target` (the gate goal nav set), then recompute every card's
/// [`CardTarget`] from its [`RingSeat`] and its ring's freshly-eased rotation.
/// The existing [`move_cards_toward_target`] then glides each card's transform to
/// that target — so the ring "spins to the gate" with no bespoke tween.
pub fn spin_rings(
    time: Res<Time>,
    mut state: ResMut<TimeWellState>,
    mut cards: Query<(&RingSeat, &mut CardTarget)>,
) {
    let alpha = 1.0 - (-RING_SPIN_EASE_RATE * time.delta_secs()).exp();
    for i in 0..super::card::N_BANDS {
        let cur = state.ring_rotation[i];
        let tgt = state.ring_rotation_target[i];
        state.ring_rotation[i] = cur + (tgt - cur) * alpha;
    }
    let rot = state.ring_rotation; // Copy [f32; N_BANDS]
    for (seat, mut target) in cards.iter_mut() {
        let r = rot[seat.band.index()];
        target.0 = super::card::ring_seat_rotated(seat.band, seat.within_index, seat.ring_len, r);
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
/// Accent bucket → linear-rgba [`Vec4`] for `WellCardMaterial.accent` (the card
/// body color the shader fills). Alpha 0.94 (matches the old vello bg).
pub fn accent_vec4(accent: &str) -> Vec4 {
    let c = accent_color(accent).to_linear();
    Vec4::new(c.red, c.green, c.blue, 0.94)
}

/// `WellCardMaterial.shape` = `[aspect (CARD_TEX_W/H = 1.6), corner_radius,
/// ring_width, inset]` in the shader's aspect-corrected UV space. Same for rim
/// and focus cards (both 1.6 aspect).
pub fn card_shape() -> Vec4 {
    Vec4::new(1.6, 0.06, 0.045, 0.012)
}

/// Shared rim-card mesh: a thin 3D block (`CARD_WIDTH × CARD_HEIGHT ×
/// [`CARD_THICKNESS`]`) whose **both** large faces render the card. A near card
/// shows its outward `+Z` face; a far card (facing away in a ring-aligned band)
/// shows its `−Z` face — front-facing from the camera's side, so it renders too
/// under default back-face culling. No double-siding needed.
///
/// UV fix: Bevy's `Cuboid` authors the `−Z` (back) face's UVs so text reads
/// upright + non-mirrored *from behind*, but the `+Z` (front) face's UVs are
/// **V-flipped** relative to the [`Rectangle`] convention the MSDF panel
/// texture is built for (`Rectangle`: uv (0,0) at the world top-left; `Cuboid`
/// front: uv (0,0) at bottom-left). Left as-is, the front/near face would show
/// text upside-down. We flip V on the front face's four vertices (the first
/// four in Bevy's cuboid vertex order) so the front matches the `Rectangle`
/// convention; the back face is already correct and untouched. Result: both
/// large faces read upright + non-mirrored with culling left at its default.
///
/// (The thin ±X/±Y side faces sample texture slivers — acceptable for an
/// 8-unit-thick block; not gold-plated.)
fn card_block_mesh() -> Mesh {
    use bevy::mesh::VertexAttributeValues;
    let mut mesh = Mesh::from(Cuboid::new(CARD_WIDTH, CARD_HEIGHT, CARD_THICKNESS));
    if let Some(VertexAttributeValues::Float32x2(uvs)) = mesh.attribute_mut(Mesh::ATTRIBUTE_UV_0) {
        // Front (+Z) face = the first four vertices in Bevy's cuboid order.
        for uv in uvs.iter_mut().take(4) {
            uv[1] = 1.0 - uv[1];
        }
    }
    mesh
}

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
