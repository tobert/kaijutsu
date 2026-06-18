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

/// Where a card wants to be. A smoothing system eases `Transform.translation`
/// toward this each frame — the "transitions are Bevy's job" stance from the
/// design doc (no transition system, just a tween on `Transform`).
#[derive(Component)]
pub struct CardTarget(pub Vec3);

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
    /// The whole well as one ordered spiral, **mouth → throat** (live → recent →
    /// haystack, each zone in its slot order — see [`super::card::spiral_order`]),
    /// rebuilt each layout tick. The single source of nav order: a card's index
    /// here is its position on the vortex *and* its odometer address (Left/Right
    /// = ±1, Up/Down = ±10, digits = the first decade at the mouth).
    pub spiral_order: Vec<ContextId>,
    /// The currently-selected card (highlighted; the target of Enter / `c`).
    pub selected: Option<ContextId>,
    /// Whether the well is *focused* on the selection: Enter (from the overview)
    /// dollies the camera into the focus card; Esc backs out; a second Enter
    /// (while focused) commits — switches to the context. Drives the camera pose
    /// in [`ease_camera_to_selection`].
    pub focused: bool,
    /// Per-context semantic-cluster assignment (id + kernel label), refreshed by
    /// the band-2 `get_clusters` poll. Drives the haystack's cluster-grouped
    /// angle and the cluster label on haystack cards. Empty when the kernel has
    /// no semantic index — band-2 then falls back to creation-id order.
    pub cluster_of: HashMap<ContextId, super::card::ClusterAssignment>,
}

impl Default for TimeWellState {
    fn default() -> Self {
        Self {
            join: kaijutsu_viz::join::Join::new(),
            entities: HashMap::new(),
            card_mesh: None,
            spiral_order: Vec::new(),
            selected: None,
            focused: false,
            cluster_of: HashMap::new(),
        }
    }
}

/// Logical card size in well units (the quad geometry). Bigger than the original
/// 64×40 so the spiral's cards read larger and closer together (1.6 aspect, to
/// match the card texture so text isn't distorted).
pub const CARD_WIDTH: f32 = 88.0;
pub const CARD_HEIGHT: f32 = 55.0;

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
/// band so it is the **throat floor** of the funnel. Lifted + tilted by the
/// shared [`super::card::well_tilt_quat`] so the spiral core sits at the low,
/// receded throat and faces up toward the camera.
const RING_DECK_DEPTH: f32 = -460.0;

/// Per-band recline (radians) layered on the billboard. With the whole funnel now
/// reclined in space (see [`super::card::WELL_TILT`]), the cards read their 3D
/// form from their *positions* and should face the camera cleanly, so the recline
/// is off. Kept as a per-band knob in case a slight lean reads better. Tunable.
fn card_tilt(band: Band) -> f32 {
    match band {
        Band::Hot => 0.0,
        Band::RecentConcluded => 0.0,
        Band::Haystack => 0.0,
    }
}

/// Exponential-smoothing rate for card motion (higher = snappier).
const CARD_EASE_RATE: f32 = 8.0;

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
    mut images: ResMut<Assets<Image>>,
    mut app_camera: Query<(Entity, &mut Camera, &mut Transform), With<Camera3d>>,
) {
    // Fresh entry always starts in the overview (not focused).
    state.focused = false;

    // Build the shared card quad once.
    if state.card_mesh.is_none() {
        state.card_mesh = Some(meshes.add(Rectangle::new(CARD_WIDTH, CARD_HEIGHT)));
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
    mut app_camera: Query<(Entity, &mut Camera), With<TimeWellCamera>>,
) {
    for e in roots
        .iter()
        .chain(cards.iter())
        .chain(reading.iter())
        .chain(decks.iter())
    {
        commands.entity(e).despawn();
    }

    state.entities.clear();
    state.focused = false;
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

/// Time-well keyboard navigation: an **odometer walk along the vortex spiral**.
/// - `0–9` — quick-jump to that index near the mouth: select + switch + exit.
/// - **Left / Right / Tab** — step ∓1 / ±1 along the spiral (one neighbour).
/// - **Up / Down** — leap ∓10 / ±10 (toward the mouth / down toward the throat).
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
    let len = state.spiral_order.len();

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

    // Index of the current selection on the spiral (None → start at the mouth).
    let current = state
        .selected
        .and_then(|sel| state.spiral_order.iter().position(|&x| x == sel));

    // Left/Right = ±1 along the spiral; Up/Down = ±10 (the odometer's tens),
    // Up toward the mouth (−), Down toward the throat (+).
    let step = if keys.just_pressed(KeyCode::ArrowUp) {
        -10
    } else if keys.just_pressed(KeyCode::ArrowDown) {
        10
    } else if keys.just_pressed(KeyCode::ArrowRight) || keys.just_pressed(KeyCode::Tab) {
        1
    } else if keys.just_pressed(KeyCode::ArrowLeft) {
        -1
    } else {
        0
    };

    if step != 0 && len > 0 {
        let idx = match current {
            Some(i) => (i as i32 + step).clamp(0, len as i32 - 1) as usize,
            None => 0, // nothing selected yet → the mouth
        };
        state.selected = state.spiral_order.get(idx).copied();
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

/// Ease the well camera between two poses, exponentially smoothed (slower than
/// the cards, so the view glides):
/// - **focused** → dolly straight in front of the focus card so it fills the
///   view (the Enter-to-focus zoom; Esc backs out).
/// - **overview** → *lean* toward the current selection (look-point + a little
///   x-parallax slide partway toward the selected card's settled `CardTarget`),
///   keeping the whole well legible. Nothing selected → the base framing.
///
/// The full dive *through* the card into the conversation is the later JOIN
/// transition; here Enter-focus only zooms to the pedestal.
pub fn ease_camera_to_selection(
    time: Res<Time>,
    state: Res<TimeWellState>,
    cards: Query<(&Card, &CardTarget)>,
    mut cam: Query<&mut Transform, With<TimeWellCamera>>,
) {
    let Ok(mut tf) = cam.single_mut() else {
        return;
    };

    let (desired_pos, desired_look) = if state.focused {
        // Dolly to a head-on framing of the focus card so it dominates the view
        // (distance tuned so it fills most of the frame without overflowing).
        (FOCUS_CARD_POS + Vec3::new(0.0, 0.0, FOCUS_DOLLY), FOCUS_CARD_POS)
    } else {
        let selected_pos = state
            .selected
            .and_then(|sel| cards.iter().find(|(c, _)| c.context_id == sel))
            .map(|(_, t)| t.0);
        match selected_pos {
            Some(p) => {
                // Lean the look-point toward the selection; nudge the camera x for
                // a touch of parallax. Fractions kept low so the overview survives.
                let look = Vec3::new(
                    p.x * 0.4,
                    CAM_BASE_LOOK.y + (p.y - CAM_BASE_LOOK.y) * 0.3,
                    CAM_BASE_LOOK.z,
                );
                let pos = Vec3::new(p.x * 0.18, CAM_BASE_POS.y, CAM_BASE_POS.z);
                (pos, look)
            }
            None => (CAM_BASE_POS, CAM_BASE_LOOK),
        }
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
        let mut rot = Transform::from_translation(tf.translation)
            .looking_at(away, Vec3::Y)
            .rotation;
        // Ring cards recline by band so they lie along the funnel slope (concept
        // mockup 27); the focus card (no `Card`) stays head-on.
        if let Some(card) = card {
            rot *= Quat::from_rotation_x(-card_tilt(card.data.band));
        }
        tf.rotation = rot;
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
