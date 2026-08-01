//! The dive's Bevy glue: entities, camera, input consumption, and the
//! per-frame sync that turns [`super::layout`]'s numbers into what you see.
//!
//! Everything geometric or graph-shaped lives in [`super::layout`] and
//! everything textual in [`super::text`]; this file only owns `Entity`,
//! `Transform`, `Visibility`, and material uniforms. That split is the same
//! one `time_well` and `fsn` draw, and for the same reason: the interesting
//! rules stay unit-testable without a `World`.
//!
//! # Entity budget
//!
//! One card entity per corpus record (~300), each with its own small MSDF
//! panel — plus exactly **one** constellation line-mesh and **one** HUD
//! panel. There is no per-edge entity and no full-graph mesh: the
//! constellation is rebuilt in place whenever the selection moves, which is
//! the concrete form principle 3 (local edge reveal) takes in the ECS.
//!
//! # Input
//!
//! Two modes, one surface (`docs/horizon-dive.md`, "Typing and moving are
//! different modes"):
//! - **navigation** — [`InputContext::HorizonDive`] bindings arrive as
//!   `ActionFired` and are consumed by [`dive_actions`]. No raw keys.
//! - **query** — the query line takes an explicit [`KeyboardGrab::HorizonQuery`]
//!   and consumes the `GrabbedKey` stream in [`dive_query_keys`], exactly the
//!   way the vi editor and the compose VimMachine do. A text field needs the
//!   whole alphabet; a grab is the sanctioned way to ask for it, and it keeps
//!   `Ctrl+A`, F1 and F12 working while you type.

use std::collections::HashSet;

use bevy::asset::RenderAssetUsages;
use bevy::mesh::PrimitiveTopology;
use bevy::prelude::*;

use crate::input::{Action, ActionFired, InputContext};
use crate::shaders::WellCardMaterial;
use crate::text::ShapingFonts;
use crate::text::msdf::{FontDataMap, MsdfAtlas, MsdfBlockGlyphs};
use crate::text::shaping::VelloFont;
use crate::ui::screen::Screen;
use crate::view::scene_palette::{ScenePalette, lin_scaled};
use crate::view::time_well::panel::{commit_panel_glyphs, create_msdf_panel};
use crate::view::time_well::scene::{accent_vec4, card_shape};

use super::corpus::{HorizonContext, HorizonRanker, SubstringRanker, synth_corpus};
use super::layout::{self, Edge, EdgeKind, SnapDir};
use super::text::{self, CARD_TEX_H, CARD_TEX_W, HUD_TEX_H, HUD_TEX_W, HudModel};

// ── Amy-tunable constants ──────────────────────────────────────────────────

/// How many synthetic contexts the spike populates the horizon with. The
/// design brief's "potentially hundreds"; also the number that makes the
/// naive answers (render every edge, list them all) visibly fail.
pub const SYNTH_COUNT: usize = 300;
/// Seed for the synthetic world. Fixed, so every run of the spike navigates
/// the *same* space — you can't evaluate spatial memory against a world that
/// reshuffles.
const SYNTH_SEED: u64 = 0x4B41_494A_5554_5355;

/// Card quad size in world units (1.6 aspect, matching the well's cards and
/// the card texture, so glyphs aren't stretched).
const CARD_W: f32 = 108.0;
const CARD_H: f32 = 67.5;

/// Resting brightness multiplier for a card the query did not light. Not
/// zero: an unlit card still has to be *there*, or the space stops being a
/// space and spatial memory goes with it.
const REST_DIM: f32 = 0.14;
/// Extra brightness for the selected card, on top of its light.
const SELECTED_DIM: f32 = 1.0;

/// Card scale at zero light and at full light. Scale carries the same signal
/// as brightness because bloom alone doesn't survive being far away.
const SCALE_DARK: f32 = 0.55;
const SCALE_LIT: f32 = 1.15;
/// Multiplier on top of that for the selection.
const SCALE_SELECTED: f32 = 1.45;

/// How far from the selection a card keeps its full brightness, and the floor
/// it fades to beyond that.
///
/// Live-tuned in (2026-08-01): the first cut had none of this, and an *empty*
/// query — which by contract lights everything (`HorizonRanker::light`) — read
/// as a solid wall of 300 fully-bright overlapping cards. The fix is not to
/// stop lighting them (they'd stop being navigable), it's to say that
/// attention has a radius: near the selection you read cards, far from it you
/// read a field.
const FOCUS_RADIUS: f32 = 2600.0;
const FOCUS_FLOOR: f32 = 0.16;

/// How far the selected card leans out of the stream toward the camera.
///
/// **Presentation, not layout.** [`layout::stream_coord`] is still the card's
/// seat and is still query-independent; this is a fixed lean applied to
/// whichever card is selected, so it can't be occluded by a neighbour that
/// happens to sit a few units nearer. It moves with the selection, never with
/// the query.
const SEL_LEAN: f32 = 130.0;

/// A card counts as "lit" — and so as a snap-navigation candidate — at or
/// above this. Below it the card is scenery.
const LIT_THRESHOLD: f32 = 0.35;

/// Camera standoff from the selection: back along +Z and up along +Y.
const CAM_BACK: f32 = 1900.0;
const CAM_UP: f32 = 520.0;
/// Exponential-smoothing rate for the camera. Slow enough to read as travel
/// (you see what you flew past), fast enough not to feel like syrup.
const CAM_EASE_RATE: f32 = 3.2;
/// Snap-and-hold epsilon so a settled camera stops writing its `Transform`.
const CAM_SETTLE_SQ: f32 = 0.05;

/// The dive's far plane. The shared app camera ships Bevy's default
/// (`far: 1000`), which frustum-culls anything further out — and the stream
/// runs to ~5200. Claimed on entry and **restored on exit** so no other
/// screen inherits it, the same claim/restore discipline `fsn::enter_fsn`
/// uses for the clear colour.
const DIVE_FAR: f32 = 30_000.0;

/// How far in front of the camera the HUD panel floats, and its quad size.
const HUD_DIST: f32 = 620.0;
const HUD_QUAD_W: f32 = 470.0;
const HUD_QUAD_H: f32 = 130.5;
/// HUD offset in camera-local space (right/up), so it sits lower-left —
/// clear of the app's own footer dock, which the dive does not hide.
const HUD_OFFSET_RIGHT: f32 = -150.0;
const HUD_OFFSET_UP: f32 = -168.0;

/// Card faces laid out per frame. 300 parley layouts in one frame is a
/// visible hitch; a budget spreads it over ~12 frames instead.
const TEXT_BUDGET: usize = 26;

/// Max query length. A query line is a query line, not a document.
const QUERY_MAX: usize = 64;

// ── Resources ──────────────────────────────────────────────────────────────

/// Whether the query line currently holds the keyboard.
///
/// Its own tiny resource rather than a field on [`HorizonDiveState`] on
/// purpose: `input::context::sync_input_context` gates on its inputs'
/// change detection, and `HorizonDiveState` changes every frame the lights
/// do — reading the mode off it would force a context re-derivation forever.
#[derive(Resource, Default, Debug, PartialEq, Eq)]
pub struct DiveTyping(pub bool);

/// Everything the dive knows. Survives across dives (the corpus is expensive
/// to rebuild and, more importantly, a space you re-enter should be the space
/// you left).
#[derive(Resource, Default)]
pub struct HorizonDiveState {
    /// The horizon corpus. Synthetic in the spike; a real slice fills this
    /// from the kernel (see `docs/horizon-dive.md`).
    pub corpus: Vec<HorizonContext>,
    /// One light per corpus entry, in corpus order. Recomputed only when the
    /// query changes.
    pub lights: Vec<f32>,
    /// The query line's text.
    pub query: String,
    /// Corpus index of the selection.
    pub selected: Option<usize>,
    /// The selection's revealed neighbourhood.
    pub edges: Vec<Edge>,
    /// Where `PopLevel` returns to — whichever screen the dive was entered
    /// from, so the debug entry doesn't have to hardcode "back to the room".
    pub origin: Screen,
    /// The camera projection the dive displaced, restored on exit.
    origin_projection: Option<Projection>,
    /// The "now" the depth axis is measured against. Captured once per dive:
    /// re-reading the wall clock every frame would make every card creep.
    pub now_ms: i64,
    /// Contexts surfaced back onto a ring by the `p` verb — hidden here,
    /// since they aren't past the horizon any more.
    pub surfaced: HashSet<usize>,
    /// Whether the key legend is showing.
    pub legend: bool,
    /// Transient feedback from the last verb.
    pub status: String,
    /// Cards whose MSDF face still needs laying out (drained on a budget).
    text_queue: Vec<usize>,
    /// Set when the query changed and the lights are stale.
    query_dirty: bool,
    /// Set when anything the HUD renders changed.
    hud_dirty: bool,
}

impl HorizonDiveState {
    /// How many contexts the current query lit.
    pub fn lit_count(&self) -> usize {
        self.lights
            .iter()
            .enumerate()
            .filter(|(i, l)| **l >= LIT_THRESHOLD && !self.surfaced.contains(i))
            .count()
    }

    /// Recompute the lights from the current query. The one place the ranker
    /// is called; swapping [`SubstringRanker`] for a real index is a one-line
    /// change here (see [`HorizonRanker`]'s own doc).
    fn relight(&mut self) {
        self.lights = SubstringRanker.light(&self.query, &self.corpus);
        self.query_dirty = false;
        self.hud_dirty = true;
    }

    /// Move the selection, recomputing its revealed neighbourhood.
    fn select(&mut self, index: Option<usize>) {
        if self.selected == index {
            return;
        }
        self.selected = index;
        self.edges = match index {
            Some(i) => layout::neighborhood(i as u32, &self.corpus),
            None => Vec::new(),
        };
        self.hud_dirty = true;
    }
}

// ── Components ─────────────────────────────────────────────────────────────

/// Root of every dive entity — despawned recursively on exit.
#[derive(Component)]
pub struct HorizonDiveRoot;

/// Marks the shared app camera while the dive owns it.
#[derive(Component)]
pub struct HorizonDiveCamera;

/// One card. `applied_*` are the last values written to the material, so the
/// per-frame sync can skip untouched cards instead of dirtying 300 material
/// assets every tick.
#[derive(Component)]
pub struct HorizonCard {
    pub index: usize,
    applied_dim: f32,
    applied_selected: bool,
    applied_edge: Option<EdgeKind>,
}

/// The single constellation mesh entity. `pub`: it names a system
/// parameter, and a `pub fn`'s signature can't leak a more-private type.
#[derive(Component)]
pub struct Constellation;

/// The single HUD panel (query line + counts + reading line).
#[derive(Component)]
pub struct DiveHud;

// ── Entry ──────────────────────────────────────────────────────────────────

/// The debug entry path: `F2` anywhere, or `h` while zoomed into the well
/// ("dive the horizon you're looking at"). Ungated — it has to be reachable
/// from whatever screen you're on — and it records where you came from so
/// `Esc` puts you back there.
pub fn handle_dive_entry(
    mut actions: MessageReader<ActionFired>,
    screen: Res<State<Screen>>,
    mut next: ResMut<NextState<Screen>>,
    mut state: ResMut<HorizonDiveState>,
) {
    for ActionFired { action, .. } in actions.read() {
        if !matches!(action, Action::DiveHorizon) {
            continue;
        }
        if *screen.get() == Screen::HorizonDive {
            continue; // already down here
        }
        state.origin = *screen.get();
        next.set(Screen::HorizonDive);
    }
}

/// Claim the camera, build the corpus if this is the first dive, and spawn
/// every entity.
pub fn enter_dive(
    mut commands: Commands,
    palette: Res<ScenePalette>,
    mut state: ResMut<HorizonDiveState>,
    mut typing: ResMut<DiveTyping>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<WellCardMaterial>>,
    mut std_materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut app_camera: Query<
        (Entity, &mut Camera, &Projection),
        (With<Camera3d>, Without<crate::view::fsn::backdrop::FsnBackdropCamera>),
    >,
    existing: Query<Entity, With<HorizonDiveRoot>>,
) {
    if !existing.is_empty() {
        return;
    }

    // A fresh dive always starts in navigation mode: dropping someone
    // straight into a text field they didn't ask for is the NMS mistake in
    // miniature (a mode you're in without having chosen it).
    typing.0 = false;

    if state.corpus.is_empty() {
        state.now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        state.corpus = synth_corpus(SYNTH_COUNT, SYNTH_SEED, state.now_ms);
        state.relight();
        // Open on the shallowest card — the thing that fell past the horizon
        // most recently, which is what you were most likely reaching for.
        let nearest = state
            .corpus
            .iter()
            .enumerate()
            .min_by_key(|(_, c)| std::cmp::Reverse(c.fell_at_ms))
            .map(|(i, _)| i);
        state.select(nearest);
    }
    state.text_queue = (0..state.corpus.len()).collect();
    state.hud_dirty = true;

    if let Ok((cam_entity, mut cam, projection)) = app_camera.single_mut() {
        state.origin_projection = Some(projection.clone());
        commands.entity(cam_entity).insert((
            HorizonDiveCamera,
            Projection::Perspective(PerspectiveProjection { far: DIVE_FAR, ..default() }),
        ));
        cam.clear_color = ClearColorConfig::Custom(Color::LinearRgba(palette.bg));
    }

    let root = commands
        .spawn((
            HorizonDiveRoot,
            Transform::default(),
            Visibility::Inherited,
            Name::new("HorizonDiveRoot"),
        ))
        .id();

    let card_mesh = meshes.add(Rectangle::new(CARD_W, CARD_H));
    for (i, ctx) in state.corpus.iter().enumerate() {
        let (image, panel) = create_msdf_panel(&mut images, CARD_TEX_W as u32, CARD_TEX_H as u32);
        let material = materials.add(WellCardMaterial {
            texture: image,
            accent: accent_vec4(&ctx.accent),
            params: Vec4::ZERO,
            shape: card_shape(),
            border: Vec4::ZERO,
            dim: Vec4::new(REST_DIM, 0.0, 0.0, 0.0),
        });
        commands.spawn((
            HorizonCard {
                index: i,
                applied_dim: REST_DIM,
                applied_selected: false,
                applied_edge: None,
            },
            Mesh3d(card_mesh.clone()),
            MeshMaterial3d(material),
            Transform::from_translation(layout::context_pos(ctx, state.now_ms))
                .with_scale(Vec3::splat(SCALE_DARK)),
            Visibility::Inherited,
            panel,
            Name::new(format!("HorizonCard{i}")),
            ChildOf(root),
        ));
    }

    // The constellation: one line-list mesh, rebuilt in place. Spawned empty.
    let constellation_mesh = meshes.add(line_list_mesh(&[], &[]));
    let constellation_material = std_materials.add(StandardMaterial {
        base_color: lin_scaled(palette.neon, palette.crest),
        unlit: true,
        ..default()
    });
    commands.spawn((
        Constellation,
        Mesh3d(constellation_mesh),
        MeshMaterial3d(constellation_material),
        Transform::default(),
        Visibility::Inherited,
        Name::new("HorizonConstellation"),
        ChildOf(root),
    ));

    let hud_mesh = meshes.add(Rectangle::new(HUD_QUAD_W, HUD_QUAD_H));
    let (hud_image, hud_panel) = create_msdf_panel(&mut images, HUD_TEX_W as u32, HUD_TEX_H as u32);
    let hud_material = materials.add(WellCardMaterial {
        texture: hud_image,
        // No body fill and no frame: the HUD is text on the void, so it never
        // occludes the space it describes.
        accent: Vec4::ZERO,
        params: Vec4::ZERO,
        shape: Vec4::new(HUD_TEX_W / HUD_TEX_H, 0.0, 0.0, 0.0),
        border: Vec4::ZERO,
        dim: Vec4::new(1.0, 0.0, 0.0, 0.0),
    });
    commands.spawn((
        DiveHud,
        Mesh3d(hud_mesh),
        MeshMaterial3d(hud_material),
        Transform::default(),
        Visibility::Inherited,
        hud_panel,
        Name::new("HorizonDiveHud"),
        ChildOf(root),
    ));

    info!(
        "horizon dive: entered ({} contexts past the horizon)",
        state.corpus.len()
    );
}

/// Despawn everything, release the camera, restore its projection.
pub fn exit_dive(
    mut commands: Commands,
    theme: Res<crate::ui::theme::Theme>,
    mut state: ResMut<HorizonDiveState>,
    mut typing: ResMut<DiveTyping>,
    roots: Query<Entity, With<HorizonDiveRoot>>,
    mut app_camera: Query<(Entity, &mut Camera), With<HorizonDiveCamera>>,
) {
    for e in roots.iter() {
        commands.entity(e).despawn();
    }
    if let Ok((cam_entity, mut cam)) = app_camera.single_mut() {
        let mut cmd = commands.entity(cam_entity);
        cmd.remove::<HorizonDiveCamera>();
        // Restore, never assume: the dive displaced whatever projection was
        // there and owes it back exactly.
        if let Some(projection) = state.origin_projection.take() {
            cmd.insert(projection);
        }
        cam.clear_color = ClearColorConfig::Custom(theme.bg);
    }
    typing.0 = false;
    state.status.clear();
    info!("horizon dive: exited");
}

// ── Input ──────────────────────────────────────────────────────────────────

/// The query line's keyboard grab. Consumes the whole `GrabbedKey` stream —
/// including when it is *not* typing, so a key that arrived on the frame the
/// mode flipped is swallowed here rather than lingering in the buffer.
pub fn dive_query_keys(
    mut keyboard: MessageReader<crate::input::events::GrabbedKey>,
    mut literal: MessageReader<crate::input::events::LiteralPrefix>,
    mut state: ResMut<HorizonDiveState>,
    mut typing: ResMut<DiveTyping>,
) {
    // `Ctrl+A a` has no meaning in a query line; drain it so it can't queue up.
    literal.read().for_each(|_| {});

    if !typing.0 {
        keyboard.read().for_each(|_| {});
        return;
    }

    for grabbed in keyboard.read() {
        let event = &grabbed.0;
        if crate::view::editor::keys::is_modifier_key(event.key_code) {
            continue;
        }
        match event.key_code {
            // Both leave the query line for navigation. They differ only in
            // intent, not effect: the query is applied live as you type, so
            // there is nothing for Enter to "submit" (see
            // `docs/horizon-dive.md`, "The query is never submitted").
            KeyCode::Escape | KeyCode::Enter | KeyCode::NumpadEnter => {
                typing.0 = false;
                state.hud_dirty = true;
            }
            KeyCode::Backspace => {
                state.query.pop();
                state.query_dirty = true;
                state.hud_dirty = true;
            }
            _ => {
                if let Some(c) = crate::view::editor::keys::pressed_char(event)
                    && !c.is_control()
                    && state.query.chars().count() < QUERY_MAX
                {
                    state.query.push(c);
                    state.query_dirty = true;
                    state.hud_dirty = true;
                }
            }
        }
    }
}

/// Navigation-mode actions: snap movement, the query-line summon, the two
/// selection verbs, the legend, and the pop out.
///
/// Filters on `context == HorizonDive` per `ActionFired`'s own contract —
/// messages buffer across frames, so a `PopLevel` fired under another context
/// one frame before the screen switch must not pop the dive you just entered.
pub fn dive_actions(
    mut actions: MessageReader<ActionFired>,
    mut state: ResMut<HorizonDiveState>,
    mut typing: ResMut<DiveTyping>,
    mut next: ResMut<NextState<Screen>>,
    mut cards: Query<(&HorizonCard, &mut Visibility)>,
    camera: Query<(&Camera, &GlobalTransform), With<HorizonDiveCamera>>,
) {
    for ActionFired { action, context } in actions.read() {
        if *context != InputContext::HorizonDive {
            continue;
        }
        match action {
            Action::PopLevel => {
                let origin = state.origin;
                next.set(origin);
            }
            Action::EditQuery => {
                typing.0 = true;
                state.hud_dirty = true;
            }
            Action::ToggleLegend => {
                state.legend = !state.legend;
                state.hud_dirty = true;
            }
            Action::StepPrev => snap(&mut state, &camera, SnapDir::Left),
            Action::StepNext => snap(&mut state, &camera, SnapDir::Right),
            Action::LevelUp => snap(&mut state, &camera, SnapDir::Up),
            Action::LevelDown => snap(&mut state, &camera, SnapDir::Down),
            Action::Activate => {
                // The prototype has nothing real to open. A slice with a live
                // corpus would `switch_context` here and leave the dive.
                let line = match state.selected.and_then(|i| state.corpus.get(i)) {
                    Some(c) => format!("open → {} (no live context in the spike)", c.title),
                    None => "open → nothing selected".to_string(),
                };
                info!("horizon dive: {line}");
                state.status = line;
                state.hud_dirty = true;
            }
            Action::Promote => {
                let Some(i) = state.selected else { continue };
                let title = state.corpus[i].title.clone();
                state.surfaced.insert(i);
                for (card, mut vis) in cards.iter_mut() {
                    if card.index == i {
                        *vis = Visibility::Hidden;
                    }
                }
                // Step off the card that just left, so the selection is never
                // parked on something invisible.
                let next_sel = state.edges.first().map(|e| e.other as usize);
                state.select(next_sel);
                state.status = format!("surfaced → {title}");
                state.hud_dirty = true;
            }
            _ => {}
        }
    }
}

/// Move the selection one snap in `dir`.
///
/// Screen-space, so "left" means what it looks like. Bevy viewport y runs
/// *down*; [`SnapDir::axis`] is stated in y-up screen space, so the y
/// component is negated on the way in — the one place that flip lives.
fn snap(
    state: &mut HorizonDiveState,
    camera: &Query<(&Camera, &GlobalTransform), With<HorizonDiveCamera>>,
    dir: SnapDir,
) {
    let Ok((cam, cam_tf)) = camera.single() else { return };
    let project = |i: usize| -> Option<Vec2> {
        let c = state.corpus.get(i)?;
        let world = layout::context_pos(c, state.now_ms);
        cam.world_to_viewport(cam_tf, world).ok().map(|v| Vec2::new(v.x, -v.y))
    };

    let candidates: Vec<(usize, Vec2)> =
        layout::nav_candidates(state.selected, &state.lights, LIT_THRESHOLD, &state.edges)
            .into_iter()
            .filter(|i| !state.surfaced.contains(i))
            .filter_map(|i| project(i).map(|p| (i, p)))
            .collect();

    // With nothing selected, a direction key means "start somewhere": take
    // the candidate nearest the middle of the frame rather than doing nothing.
    let Some(sel) = state.selected else {
        let start = candidates.iter().min_by(|a, b| {
            a.1.length_squared().partial_cmp(&b.1.length_squared()).unwrap_or(std::cmp::Ordering::Equal)
        });
        if let Some((i, _)) = start {
            let i = *i;
            state.select(Some(i));
        }
        return;
    };
    let Some(from) = project(sel) else { return };
    if let Some(hit) = layout::snap_neighbor(from, &candidates, dir) {
        state.select(Some(hit));
    }
}

// ── Per-frame sync ─────────────────────────────────────────────────────────

/// Recompute lights when the query changed. Guarded, not per-frame: relighting
/// 300 records is cheap but it also dirties every card's material downstream.
pub fn apply_query(mut state: ResMut<HorizonDiveState>) {
    if state.query_dirty {
        state.relight();
        // A selection that just went dark is still a fine place to stand —
        // the selection is where you *are*, not a search result. Deliberately
        // no re-selection here (`docs/horizon-dive.md`, "The query never
        // moves you").
    }
}

/// Ease the camera toward a standoff pose on the selection. There is no free
/// flight: the camera is a consequence of the selection, never an input
/// (principle 1).
pub fn ease_dive_camera(
    time: Res<Time>,
    state: Res<HorizonDiveState>,
    mut camera: Query<&mut Transform, With<HorizonDiveCamera>>,
) {
    let Ok(mut tf) = camera.single_mut() else { return };
    let target = match state.selected.and_then(|i| state.corpus.get(i)) {
        Some(c) => layout::context_pos(c, state.now_ms),
        None => Vec3::ZERO,
    };
    let eye = target + Vec3::new(0.0, CAM_UP, CAM_BACK);
    let want = Transform::from_translation(eye).looking_at(target, Vec3::Y);

    let t = 1.0 - (-CAM_EASE_RATE * time.delta_secs()).exp();
    if tf.translation.distance_squared(want.translation) < CAM_SETTLE_SQ {
        if tf.translation != want.translation {
            *tf = want;
        }
        return;
    }
    tf.translation = tf.translation.lerp(want.translation, t);
    tf.rotation = tf.rotation.slerp(want.rotation, t);
}

/// Face every card at the camera. Cards, not stars — a card read edge-on is
/// a line.
pub fn billboard_cards(
    camera: Query<&Transform, (With<HorizonDiveCamera>, Without<HorizonCard>)>,
    mut cards: Query<&mut Transform, With<HorizonCard>>,
) {
    let Ok(cam_tf) = camera.single() else { return };
    for mut tf in cards.iter_mut() {
        if tf.rotation != cam_tf.rotation {
            tf.rotation = cam_tf.rotation;
        }
    }
}

/// Write each card's brightness, scale, and ring state — but only where they
/// actually changed, so a resting field doesn't dirty 300 material assets
/// every frame.
pub fn sync_card_visuals(
    state: Res<HorizonDiveState>,
    palette: Res<ScenePalette>,
    mut materials: ResMut<Assets<WellCardMaterial>>,
    mut cards: Query<(&mut HorizonCard, &mut Transform, &MeshMaterial3d<WellCardMaterial>)>,
) {
    // The selection's seat — the anchor the focus falloff measures against.
    let focus = state
        .selected
        .and_then(|i| state.corpus.get(i))
        .map(|c| layout::context_pos(c, state.now_ms));

    for (mut card, mut tf, mat_node) in cards.iter_mut() {
        let i = card.index;
        let light = if state.surfaced.contains(&i) {
            0.0
        } else {
            state.lights.get(i).copied().unwrap_or(0.0)
        };
        let selected = state.selected == Some(i);
        let edge = state.edges.iter().find(|e| e.other as usize == i).map(|e| e.kind);
        let Some(ctx) = state.corpus.get(i) else { continue };
        let seat = layout::context_pos(ctx, state.now_ms);

        // Attention falls off with distance from the selection. A linked card
        // is exempt: the constellation is what you asked to see, and a
        // drift partner half the stream away is exactly the link worth
        // noticing.
        let atten = match focus {
            Some(f) if edge.is_none() && !selected => {
                (1.0 - seat.distance(f) / FOCUS_RADIUS).clamp(FOCUS_FLOOR, 1.0)
            }
            _ => 1.0,
        };

        let dim = if selected {
            SELECTED_DIM
        } else {
            (REST_DIM + (1.0 - REST_DIM) * light) * atten
        };
        let scale = (SCALE_DARK + (SCALE_LIT - SCALE_DARK) * light)
            * if selected { SCALE_SELECTED } else { 1.0 };

        // Seat + (for the selection only) a fixed lean toward the camera, so
        // the card you are reading is never occluded by a neighbour a few
        // units nearer. See [`SEL_LEAN`]: presentation, not layout.
        let want_pos = if selected { seat + Vec3::Z * SEL_LEAN } else { seat };
        if tf.translation != want_pos {
            tf.translation = want_pos;
        }
        let want_scale = Vec3::splat(scale);
        if tf.scale != want_scale {
            tf.scale = want_scale;
        }

        if (card.applied_dim - dim).abs() < 1e-3
            && card.applied_selected == selected
            && card.applied_edge == edge
        {
            continue;
        }
        card.applied_dim = dim;
        card.applied_selected = selected;
        card.applied_edge = edge;

        if let Some(mat) = materials.get_mut(&mat_node.0) {
            mat.dim = Vec4::new(dim, 0.0, 0.0, 0.0);
            // `params` = [selected, in_lineage, status, drifting]: the well's
            // shader already draws a selection ring and a lineage ring, so
            // lineage-shaped edges reuse the lineage ring and drift reuses
            // the drift sheen. No new shader for the dive.
            let in_lineage = matches!(edge, Some(EdgeKind::Parent | EdgeKind::Child | EdgeKind::Sibling));
            let drifting = matches!(edge, Some(EdgeKind::Drift));
            mat.params = Vec4::new(
                if selected { 1.0 } else { 0.0 },
                if in_lineage { 1.0 } else { 0.0 },
                0.0,
                if drifting { 1.0 } else { 0.0 },
            );
            mat.border = match edge {
                Some(kind) => (edge_color(kind, &palette) * palette.trim).extend(1.0),
                None => Vec4::ZERO,
            };
        }
    }
}

/// Rebuild the constellation whenever the selection moves. One mesh, one
/// segment per revealed edge, vertex-coloured by kind.
pub fn sync_constellation(
    state: Res<HorizonDiveState>,
    palette: Res<ScenePalette>,
    mut meshes: ResMut<Assets<Mesh>>,
    constellation: Query<&Mesh3d, With<Constellation>>,
    // `(selection, surfaced count)` — the only two inputs the mesh depends
    // on. Gating on `state.is_changed()` instead would rebuild every frame:
    // the HUD's own dirty flag lives on the same resource.
    mut last: Local<Option<(Option<usize>, usize)>>,
) {
    let key = (state.selected, state.surfaced.len());
    if *last == Some(key) {
        return;
    }
    *last = Some(key);

    let Ok(mesh3d) = constellation.single() else { return };
    let Some(mesh) = meshes.get_mut(&mesh3d.0) else { return };

    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut colors: Vec<[f32; 4]> = Vec::new();
    if let Some(sel) = state.selected.and_then(|i| state.corpus.get(i)) {
        let from = layout::context_pos(sel, state.now_ms);
        for e in &state.edges {
            let Some(other) = state.corpus.get(e.other as usize) else { continue };
            if state.surfaced.contains(&(e.other as usize)) {
                continue;
            }
            let to = layout::context_pos(other, state.now_ms);
            let c = edge_color(e.kind, &palette) * palette.trim;
            positions.push(from.to_array());
            positions.push(to.to_array());
            // Bright at the selection, fading outward: the constellation
            // should read as "reaching from here", not as a free-floating web.
            colors.push([c.x, c.y, c.z, 1.0]);
            colors.push([c.x * 0.35, c.y * 0.35, c.z * 0.35, 1.0]);
        }
    }
    *mesh = line_list_mesh(&positions, &colors);
}

/// Lay out card faces on a budget so the first frame of a dive doesn't hitch.
pub fn build_card_text(
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    mut state: ResMut<HorizonDiveState>,
    mut cards: Query<(&HorizonCard, &mut MsdfBlockGlyphs)>,
) {
    if state.text_queue.is_empty() {
        return;
    }
    let Some(font) = fonts.get(&font_handles.mono) else {
        return; // font still loading — the queue keeps its work for later
    };
    let Some(atlas) = atlas.as_deref_mut() else {
        return;
    };

    let take = TEXT_BUDGET.min(state.text_queue.len());
    let batch: Vec<usize> = state.text_queue.drain(..take).collect();
    let wanted: HashSet<usize> = batch.iter().copied().collect();
    for (card, mut msdf) in cards.iter_mut() {
        if !wanted.contains(&card.index) {
            continue;
        }
        let Some(ctx) = state.corpus.get(card.index) else { continue };
        let glyphs = text::card_glyphs(ctx, font, atlas, &mut font_data_map);
        commit_panel_glyphs(&mut msdf, glyphs);
    }
}

/// Park the HUD in front of the camera and refresh its text when anything it
/// reports has changed.
pub fn sync_hud(
    fonts: Res<Assets<VelloFont>>,
    font_handles: Res<ShapingFonts>,
    mut atlas: Option<ResMut<MsdfAtlas>>,
    mut font_data_map: ResMut<FontDataMap>,
    typing: Res<DiveTyping>,
    mut state: ResMut<HorizonDiveState>,
    camera: Query<&Transform, (With<HorizonDiveCamera>, Without<DiveHud>)>,
    mut hud: Query<(&mut Transform, &mut MsdfBlockGlyphs), With<DiveHud>>,
) {
    let Ok((mut tf, mut msdf)) = hud.single_mut() else { return };

    if let Ok(cam_tf) = camera.single() {
        let pose = Transform::from_translation(
            cam_tf.translation
                + cam_tf.forward() * HUD_DIST
                + cam_tf.right() * HUD_OFFSET_RIGHT
                + cam_tf.up() * HUD_OFFSET_UP,
        )
        .with_rotation(cam_tf.rotation);
        if *tf != pose {
            *tf = pose;
        }
    }

    if !state.hud_dirty {
        return;
    }
    let Some(font) = fonts.get(&font_handles.mono) else { return };
    let Some(atlas) = atlas.as_deref_mut() else { return };

    let model = HudModel {
        query: &state.query,
        typing: typing.0,
        lit: state.lit_count(),
        total: state.corpus.len() - state.surfaced.len(),
        selected: state.selected.and_then(|i| state.corpus.get(i)),
        edges: state.edges.len(),
        status: &state.status,
        legend: state.legend,
    };
    let glyphs = text::hud_glyphs(&model, font, atlas, &mut font_data_map);
    commit_panel_glyphs(&mut msdf, glyphs);
    state.hud_dirty = false;
}

// ── Small helpers ──────────────────────────────────────────────────────────

/// Hue for an edge kind. Lineage rides the well's own neon/terrace family;
/// drift gets gold, matching the well's "drift = shimmer" bling.
fn edge_color(kind: EdgeKind, palette: &ScenePalette) -> Vec3 {
    match kind {
        EdgeKind::Parent => ScenePalette::vec3(palette.neon),
        EdgeKind::Child => ScenePalette::vec3(palette.terrace),
        EdgeKind::Sibling => ScenePalette::vec3(palette.violet_thread),
        EdgeKind::Drift => ScenePalette::vec3(palette.gold),
    }
}

/// A `LineList` mesh with per-vertex colours. `colors.len()` must equal
/// `positions.len()`. (`fsn::scene` has a `pub(super)` twin; a spike is not a
/// good enough reason to widen another module's visibility, so this is a
/// local copy — `docs/horizon-dive.md` lists de-duplicating it as cleanup.)
fn line_list_mesh(positions: &[[f32; 3]], colors: &[[f32; 4]]) -> Mesh {
    debug_assert_eq!(
        positions.len(),
        colors.len(),
        "one vertex colour per position, or the attribute is silently misread"
    );
    let normals = vec![[0.0, 0.0, 1.0]; positions.len()];
    Mesh::new(PrimitiveTopology::LineList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, positions.to_vec())
        .with_inserted_attribute(Mesh::ATTRIBUTE_NORMAL, normals)
        .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, colors.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lit_count_ignores_dark_and_surfaced_cards() {
        let mut state = HorizonDiveState {
            lights: vec![1.0, 0.1, 0.9, 0.8],
            ..default()
        };
        assert_eq!(state.lit_count(), 3);
        state.surfaced.insert(2);
        assert_eq!(state.lit_count(), 2, "a surfaced card is no longer past the horizon");
    }

    #[test]
    fn selecting_recomputes_the_revealed_neighbourhood() {
        let mut state = HorizonDiveState {
            corpus: synth_corpus(60, 1, 1_800_000_000_000),
            ..default()
        };
        state.relight();
        state.select(Some(5));
        assert_eq!(state.selected, Some(5));
        assert_eq!(state.edges, layout::neighborhood(5, &state.corpus));
        state.select(None);
        assert!(state.edges.is_empty(), "no selection reveals no edges");
    }

    #[test]
    fn relight_marks_the_hud_dirty_and_clears_the_query_flag() {
        let mut state = HorizonDiveState {
            corpus: synth_corpus(20, 1, 1_800_000_000_000),
            query_dirty: true,
            ..default()
        };
        state.relight();
        assert!(!state.query_dirty);
        assert!(state.hud_dirty);
        assert_eq!(state.lights.len(), 20);
    }

    #[test]
    fn the_query_never_moves_the_selection() {
        // The invariant the whole "query as light" stance rests on: relighting
        // must not touch where you are standing, even if you go dark.
        let mut state = HorizonDiveState {
            corpus: synth_corpus(60, 1, 1_800_000_000_000),
            ..default()
        };
        state.relight();
        state.select(Some(7));
        let before = state.selected;
        state.query = "zzzznothingmatchesthis".into();
        state.relight();
        assert_eq!(state.selected, before, "a query that lights nothing must not evict you");
        assert_eq!(state.lit_count(), 0);
    }
}
