//! The south wall's switchboard: a grid of presence lamps, one per
//! non-archived kernel context, reading as a machine-room status wall from
//! across the room (`shell.md`'s old "future MCP broker switchboard"
//! placeholder — built here). Dense warm glow = busy fleet, dark = everyone
//! asleep; no lamp ever needs to be legible at room scale, only its
//! brightness/color/motion.
//!
//! **Design split** (mirrors [`super::activity`]'s stance): the recency
//! curve, the roster selection/cap, the grid layout math, and the
//! ember/flash state machine are pure — no Bevy types, unit-tested below.
//! Only the spawn/reconcile/sync systems at the bottom touch `bevy`,
//! [`kaijutsu_client::ContextInfo`], or [`kaijutsu_client::ServerEvent`].
//!
//! **CPU-driven, not `TraceGlowMaterial`.** Every other faint moving glow in
//! this room (`super`'s module doc) rides [`crate::shaders::TraceGlowMaterial`]
//! for zero-CPU, GPU-time-only animation. A lamp can't: its brightness/hue
//! depend on per-context state that changes at *poll* and *event* cadence
//! (recency decay, a sticky error ember, a completion flash), not on
//! `globals.time` alone. So lamps stay plain unlit [`StandardMaterial`]s,
//! written from the CPU each frame — quantized through [`super::quantize`]/
//! [`super::set_glow`] (the well's settled-material discipline) so an idle
//! lamp population stops touching `Assets<StandardMaterial>` once its
//! recency curve has flattened and no flash is in flight. `Running`'s
//! "breathing" is still `trace_glow.wgsl`'s mode-1 sine formula
//! ([`breathing_multiplier`]) — just evaluated on the CPU against
//! `Time::elapsed_secs()` instead of `globals.time`, so it can be summed with
//! the other per-frame state before one material write.
//!
//! **Slots, not entities.** [`LAMP_CAP`] lamp tiles spawn once per room visit
//! at fixed grid positions ([`lamp_grid_local_xy`]) and are never
//! spawned/despawned again — a context taking or leaving the visible roster
//! only changes which context a slot's material reflects
//! ([`SwitchboardState::order`]), never the entity count. An unfilled slot
//! (fewer than [`LAMP_CAP`] live contexts) just renders fully off — visually
//! identical to "very inactive," which is the intended read for an empty
//! socket in a status-lamp wall.

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

use bevy::prelude::*;
use kaijutsu_client::{ContextInfo, ServerEvent};
use kaijutsu_types::{ContextId, Status};

use super::bearing::{self, Bearing};
use crate::connection::actor_plugin::ServerEventMessage;
use crate::connection::drift::DriftState;
use crate::view::scene_geometry;
use crate::view::scene_palette::ScenePalette;

// ── Tunables (Amy-tunable — first guess, live-tune over BRP) ────────────────

/// Cap on lamps ever shown — the mission brief's number: "most-recently-
/// active first; if more exist, that's fine, they just don't get lamps yet."
pub const LAMP_CAP: usize = 24;
const LAMP_GRID_COLS: usize = 6;
const LAMP_GRID_ROWS: usize = 4;
const _: () = assert!(
    LAMP_GRID_COLS * LAMP_GRID_ROWS == LAMP_CAP,
    "the grid must have exactly LAMP_CAP cells — lamp_grid_local_xy assumes a full rectangle"
);

/// Recency brightness never fully reaches zero (a lamp with a context behind
/// it always reads as present, however faint) and never fully saturates
/// below [`RECENCY_TAU_SECS`]'s time constant. See [`recency_brightness`].
const RECENCY_FLOOR: f32 = 0.04;
const RECENCY_TAU_SECS: f32 = 900.0;

/// How long a `TurnCompleted` flash takes to decay to nothing, and how much
/// extra brightness it pops on top of the base color at its peak (mission:
/// "a short bright flash that decays over ~2-3s").
const FLASH_DURATION_SECS: f32 = 2.5;
const FLASH_PEAK_BOOST: f32 = 1.6;

/// Steady brightness for a sticky red ember — deliberately NOT scaled by
/// recency (mission: "Bright is fine — Amy wants to be able to notice it
/// from another screen"); an old, still-unresolved error must read exactly
/// as loud as a fresh one.
const ERROR_BRIGHTNESS: f32 = 1.35;

/// `Running`'s breathing modulation — the same sine shape as
/// `assets/shaders/trace_glow.wgsl`'s mode 1, evaluated on the CPU
/// ([`breathing_multiplier`]'s own doc has why).
const BREATH_RATE: f32 = 1.4;
const BREATH_TROUGH: f32 = 0.62;

/// Lamp tile size and grid gap (world units) and how far it stands proud of
/// the wall panel — a touch more than [`super::WALL_THREAD_PROUD`]'s, the
/// panel's other foreground-content idiom, for the same reason: lamps are
/// this panel's featured content, not its trim.
const LAMP_SIZE: f32 = 46.0;
const LAMP_GAP: f32 = 14.0;
const LAMP_PROUD: f32 = 3.0;

/// A saturated red-orange coal — distinct from the warm gold base at a
/// glance, the mission's "clearly distinct across the room."
const ERROR_HUE: [f32; 3] = [1.0, 0.12, 0.02];
/// Gold-white a completion flash blends toward at its peak.
const FLASH_HUE: [f32; 3] = [1.0, 0.95, 0.85];

// ── Pure: recency, roster selection, grid layout ─────────────────────────────

/// The activity timestamp a lamp's recency reads: `last_activity_at` when
/// the kernel has one, else `created_at` — the same rule
/// `time_well::card`'s (private) `effective_activity` uses; duplicated
/// rather than exported cross-module since it's a two-line rule, not a
/// shared abstraction worth a public seam.
fn effective_activity(info: &ContextInfo) -> u64 {
    info.last_activity_at.unwrap_or(info.created_at)
}

/// Non-archived contexts, most-recently-active first, capped at `cap` — the
/// switchboard's lamp roster. Ties break on [`ContextId`] ordering so two
/// contexts sharing a timestamp don't reshuffle frame to frame as poll
/// results happen to arrive in a different order.
pub fn select_lamp_contexts(contexts: &[ContextInfo], cap: usize) -> Vec<ContextId> {
    let mut live: Vec<&ContextInfo> = contexts.iter().filter(|c| !c.archived).collect();
    live.sort_by(|a, b| effective_activity(b).cmp(&effective_activity(a)).then_with(|| a.id.cmp(&b.id)));
    live.truncate(cap);
    live.into_iter().map(|c| c.id).collect()
}

/// Brightness 0..1 from age alone, before the `Running`/error/flash overlays
/// — a smooth exponential decay, never a step: bright just after activity
/// (age 0 → 1.0), still reads warm at a minute (≈0.94), roughly the middle
/// of the curve at ten minutes (≈0.53), down near the floor by an hour
/// (≈0.06), asymptoting toward [`RECENCY_FLOOR`] for anything older — a
/// lamp never goes fully dark just from time passing.
pub fn recency_brightness(age_secs: f32) -> f32 {
    let age = age_secs.max(0.0);
    RECENCY_FLOOR + (1.0 - RECENCY_FLOOR) * (-age / RECENCY_TAU_SECS).exp()
}

/// `trace_glow.wgsl`'s mode-1 breathing formula, evaluated on the CPU: a
/// uniform sine between [`BREATH_TROUGH`] and 1.0. `t` is elapsed seconds
/// (`Time::elapsed_secs()`), `phase` a per-lamp offset (`bearing::hash01`)
/// so a full lamp wall doesn't breathe in lockstep.
pub fn breathing_multiplier(t: f32, phase: f32) -> f32 {
    let breath = 0.5 * (1.0 + (t * BREATH_RATE + phase).sin());
    BREATH_TROUGH + (1.0 - BREATH_TROUGH) * breath
}

/// Local `(x, y)` offset from the panel's own center for lamp slot `index`
/// (row-major, left→right, top→bottom, [`LAMP_GRID_COLS`] ×
/// [`LAMP_GRID_ROWS`]) — a FIXED grid of slot positions, not a pack-left
/// layout: a lamp's screen position never moves when a *different* context
/// takes or leaves an unrelated slot, only the roster changes on reconcile.
pub fn lamp_grid_local_xy(index: usize, cell: f32, gap: f32) -> (f32, f32) {
    let cols = LAMP_GRID_COLS as f32;
    let rows = LAMP_GRID_ROWS as f32;
    let col = (index % LAMP_GRID_COLS) as f32;
    let row = (index / LAMP_GRID_COLS) as f32;
    let total_w = cols * cell + (cols - 1.0) * gap;
    let total_h = rows * cell + (rows - 1.0) * gap;
    let x = -total_w / 2.0 + col * (cell + gap) + cell / 2.0;
    let y = total_h / 2.0 - row * (cell + gap) - cell / 2.0;
    (x, y)
}

// ── Pure: ember/flash state machine ──────────────────────────────────────────

/// Per-context lamp runtime: the sticky error ember and any in-flight
/// completion flash. Lives in [`SwitchboardState::signals`], keyed by
/// [`ContextId`] — independent of which (if any) grid slot the context
/// currently occupies, so state survives a context briefly scrolling off
/// the visible roster and back on (`SwitchboardState::retain_relevant`).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct LampSignal {
    /// Set by [`LampSignal::on_turn_failed`] or a polled [`Status::Error`]
    /// ([`LampSignal::reconcile_status`]); cleared only by
    /// [`LampSignal::on_turn_completed`] or a poll showing the context has
    /// moved past `Error` — never by time alone. That's the "sticky" in
    /// sticky ember (mission: "STAYS red until the context shows fresh
    /// successful activity").
    pub sticky_error: bool,
    /// Seconds elapsed since a `TurnCompleted` flash started, or `None`
    /// between flashes / once one has fully decayed past
    /// [`FLASH_DURATION_SECS`].
    flash_elapsed: Option<f32>,
}

impl LampSignal {
    /// `ServerEvent::TurnCompleted` — fresh successful activity: clears any
    /// sticky ember and starts a new flash from its peak, even if one was
    /// already decaying (a turn finishing rapid-fire re-pops the flash
    /// rather than blending two partial ones).
    pub fn on_turn_completed(&mut self) {
        self.sticky_error = false;
        self.flash_elapsed = Some(0.0);
    }

    /// `ServerEvent::TurnFailed` — sets the sticky ember. A failure is the
    /// opposite of a flash: cancel any flash in flight so it can't paint
    /// over the ember that should dominate from this instant on.
    pub fn on_turn_failed(&mut self) {
        self.sticky_error = true;
        self.flash_elapsed = None;
    }

    /// Fold in the kernel's own polled `live_status` (every `DriftState`
    /// refresh) — the poll-driven half of ember clearing, covering a context
    /// that was already erroring before this lamp ever saw a `TurnFailed`
    /// push (e.g. the app just opened): `Error` sets the sticky ember,
    /// `Running`/`Pending`/`Done` all count as "fresh successful activity"
    /// and clear it. Exhaustive match, no wildcard — a future `Status`
    /// variant must be given an explicit clearing verdict here rather than
    /// silently inheriting one.
    pub fn reconcile_status(&mut self, live_status: Status) {
        self.sticky_error = match live_status {
            Status::Error => true,
            Status::Running | Status::Pending | Status::Done => false,
        };
    }

    /// Advance the flash decay by `dt` seconds.
    pub fn tick(&mut self, dt: f32) {
        if let Some(e) = self.flash_elapsed.as_mut() {
            *e += dt;
            if *e >= FLASH_DURATION_SECS {
                self.flash_elapsed = None;
            }
        }
    }

    /// 1.0 at the instant of a `TurnCompleted`, decaying linearly to 0.0 by
    /// [`FLASH_DURATION_SECS`]; 0.0 with no flash in flight.
    pub fn flash_intensity(&self) -> f32 {
        match self.flash_elapsed {
            None => 0.0,
            Some(e) => (1.0 - e / FLASH_DURATION_SECS).clamp(0.0, 1.0),
        }
    }

    /// Whether this signal carries state worth keeping even if its context
    /// has scrolled off the visible lamp roster.
    pub fn is_active(&self) -> bool {
        self.sticky_error || self.flash_elapsed.is_some()
    }
}

// ── Pure: brightness/hue combination ─────────────────────────────────────────

/// The full per-frame brightness combination for one lamp: a sticky error
/// overrides everything else with a steady bright coal; otherwise recency
/// sets the base, `Running` breathes on top of it, and an in-flight
/// completion flash adds an on-top pop.
pub fn lamp_brightness(
    age_secs: f32,
    running: bool,
    sticky_error: bool,
    flash_intensity: f32,
    breath_t: f32,
    breath_phase: f32,
) -> f32 {
    if sticky_error {
        return ERROR_BRIGHTNESS;
    }
    let mut b = recency_brightness(age_secs);
    if running {
        b *= breathing_multiplier(breath_t, breath_phase);
    }
    b + flash_intensity * FLASH_PEAK_BOOST
}

/// Lamp hue for this frame: `warm` blended toward `flash_hue` by
/// `flash_intensity` (a completion pop turns gold-white, then eases back),
/// or `error_hue` outright once `sticky_error` — a distinct saturated coal,
/// not a tint of the warm base, so it reads across the room at a glance.
pub fn lamp_hue(
    sticky_error: bool,
    flash_intensity: f32,
    warm: [f32; 3],
    flash_hue: [f32; 3],
    error_hue: [f32; 3],
) -> [f32; 3] {
    if sticky_error {
        return error_hue;
    }
    let t = flash_intensity.clamp(0.0, 1.0);
    [
        warm[0] + (flash_hue[0] - warm[0]) * t,
        warm[1] + (flash_hue[1] - warm[1]) * t,
        warm[2] + (flash_hue[2] - warm[2]) * t,
    ]
}

// ── Bevy glue ─────────────────────────────────────────────────────────────────

/// One occupied lamp slot: which context it shows and the polled facts its
/// per-frame brightness needs (`effective_activity_ms` for the recency
/// curve, `running` for the breathing overlay). Rebuilt at poll cadence in
/// [`reconcile_switchboard`]; `Copy` so a lamp can read its own entry out of
/// [`SwitchboardState::order`] without holding a borrow across the
/// `signals` lookup that follows it.
#[derive(Debug, Clone, Copy)]
struct LampEntry {
    context_id: ContextId,
    effective_activity_ms: u64,
    running: bool,
}

/// Live switchboard roster + per-context ember/flash runtime. See this
/// module's header doc for the slots-not-entities model `order` implements.
#[derive(Resource, Default)]
pub(crate) struct SwitchboardState {
    /// `order[slot]` is the context lighting that grid slot this poll, or
    /// absent if fewer than [`LAMP_CAP`] contexts are live.
    order: Vec<LampEntry>,
    /// Ember/flash runtime per context, independent of current slot
    /// occupancy (`retain_relevant`'s doc).
    signals: HashMap<ContextId, LampSignal>,
}

impl SwitchboardState {
    /// Drop signal entries for contexts that are both off the visible
    /// roster AND carrying no live state ([`LampSignal::is_active`]) — keeps
    /// the map bounded across a long session without losing an ember/flash
    /// for a context that's merely between polls.
    fn retain_relevant(&mut self) {
        let seated: std::collections::HashSet<ContextId> =
            self.order.iter().map(|e| e.context_id).collect();
        self.signals.retain(|id, sig| seated.contains(id) || sig.is_active());
    }
}

/// One lamp tile at a fixed grid slot (`0..LAMP_CAP`) on the switchboard
/// panel — spawned once per room visit ([`spawn_switchboard`]) and never
/// despawned/respawned as contexts come and go; only its material changes
/// ([`sync_switchboard_glow`]).
#[derive(Component)]
pub(crate) struct SwitchboardLamp(usize);

/// The south octagon panel's room-space transform — the same `looking_at`
/// composition `super::spawn_walls` uses for every cardinal panel, derived
/// independently here (not shared state) since [`spawn_switchboard`] runs as
/// a plain helper, not a system, and has no panel entity to read a
/// `Transform` off of.
fn south_panel_transform() -> Transform {
    let panels = bearing::octagon_panels(scene_geometry::WALL_APOTHEM);
    let panel = panels
        .iter()
        .find(|p| p.bearing == Some(Bearing::South))
        .expect("south is one of the eight octagon panels");
    let pos = Vec3::new(panel.center[0], super::WALL_HEIGHT * 0.5, panel.center[2]);
    let outward = Vec3::new(pos.x * 2.0, pos.y, pos.z * 2.0);
    Transform::from_translation(pos).looking_at(outward, Vec3::Y)
}

/// Spawn the switchboard's [`LAMP_CAP`] lamp-tile slots on the south wall
/// panel, each starting fully off (`Vec3::ZERO` — [`sync_switchboard_glow`]
/// lights them from [`SwitchboardState`] every frame after). Called from
/// `enter_room` (`super`) with resources it already holds — see the call
/// site's own comment for why no new system params land on `enter_room`
/// itself.
pub(crate) fn spawn_switchboard(
    commands: &mut Commands,
    root: Entity,
    meshes: &mut Assets<Mesh>,
    mats: &mut Assets<StandardMaterial>,
) {
    let panel_tf = south_panel_transform();
    let mesh = meshes.add(Rectangle::new(LAMP_SIZE, LAMP_SIZE));
    for i in 0..LAMP_CAP {
        let (x, y) = lamp_grid_local_xy(i, LAMP_SIZE, LAMP_GAP);
        let local = Vec3::new(x, y, LAMP_PROUD);
        let mat = mats.add(super::unlit(super::lin_v(Vec3::ZERO)));
        commands.spawn((
            SwitchboardLamp(i),
            super::RoomDistraction,
            Mesh3d(mesh.clone()),
            MeshMaterial3d(mat),
            Transform::from_translation(panel_tf.transform_point(local))
                .with_rotation(panel_tf.rotation),
            Visibility::Inherited,
            Name::new(format!("SwitchboardLamp{i}")),
            ChildOf(root),
        ));
    }
}

/// Reconcile the lamp roster against the latest `DriftState` poll — gated on
/// `drift.is_changed()` so a quiet poll costs nothing (`time_well::sync`'s
/// own gate on the same resource). Rebuilds [`SwitchboardState::order`] and
/// folds each *seated* context's freshly-polled `live_status` into its
/// [`LampSignal`] (the poll-driven half of ember clearing;
/// `ingest_switchboard_events` below is the push-driven half). Runs
/// **ungated** across every screen (`RoomPlugin`'s registration) so the
/// roster stays current while you're elsewhere, same as `ingest_room_activity`.
pub(crate) fn reconcile_switchboard(drift: Res<DriftState>, mut state: ResMut<SwitchboardState>) {
    if !drift.is_changed() {
        return;
    }
    let selected = select_lamp_contexts(&drift.contexts, LAMP_CAP);
    let by_id: HashMap<ContextId, &ContextInfo> = drift.contexts.iter().map(|c| (c.id, c)).collect();
    state.order = selected
        .into_iter()
        .filter_map(|id| {
            by_id.get(&id).map(|c| LampEntry {
                context_id: id,
                effective_activity_ms: effective_activity(c),
                running: c.live_status == Status::Running,
            })
        })
        .collect();
    for c in &drift.contexts {
        if let Some(sig) = state.signals.get_mut(&c.id) {
            sig.reconcile_status(c.live_status);
        }
    }
    state.retain_relevant();
}

/// Drain `TurnCompleted`/`TurnFailed` into per-context lamp signals — the
/// push-driven half of the ember/flash state machine
/// ([`reconcile_switchboard`] is the poll-driven half). Runs **ungated**
/// (every screen, `RoomPlugin`'s registration) — the same
/// `time_well::live::ingest_live_events` stance: a flash or an ember must
/// not be lost just because the switchboard wasn't on screen when it
/// happened.
pub(crate) fn ingest_switchboard_events(
    mut events: MessageReader<ServerEventMessage>,
    mut state: ResMut<SwitchboardState>,
) {
    for ServerEventMessage(ev) in events.read() {
        match ev {
            ServerEvent::TurnCompleted { context_id, .. } => {
                state.signals.entry(*context_id).or_default().on_turn_completed();
            }
            ServerEvent::TurnFailed { context_id, .. } => {
                state.signals.entry(*context_id).or_default().on_turn_failed();
            }
            _ => {}
        }
    }
}

/// Advance every lamp signal's flash decay and push each seated lamp's
/// resulting brightness/hue into its material; an unfilled slot is driven
/// fully off. Quantized before writing
/// (`super::quantize`/`super::set_glow`) so a settled lamp population stops
/// touching `Assets<StandardMaterial>` once flashes have decayed and the
/// recency curve has flattened.
pub(crate) fn sync_switchboard_glow(
    time: Res<Time>,
    palette: Res<ScenePalette>,
    mut state: ResMut<SwitchboardState>,
    mut mats: ResMut<Assets<StandardMaterial>>,
    lamps: Query<(&SwitchboardLamp, &MeshMaterial3d<StandardMaterial>)>,
) {
    let dt = time.delta_secs();
    for sig in state.signals.values_mut() {
        sig.tick(dt);
    }

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let t = time.elapsed_secs();
    let warm = ScenePalette::vec3(palette.gold).to_array();

    for (lamp, handle) in lamps.iter() {
        let Some(entry) = state.order.get(lamp.0).copied() else {
            super::set_glow(&mut mats, &handle.0, Vec3::ZERO);
            continue;
        };
        let sig = state.signals.entry(entry.context_id).or_default();
        let age_secs = now_ms.saturating_sub(entry.effective_activity_ms) as f32 / 1000.0;
        // Per-lamp phase offset so a full wall of `Running` lamps doesn't
        // breathe in lockstep — same `hash01` desync idiom `spawn_walls`
        // uses for its trim phases.
        let phase = bearing::hash01(lamp.0 as u32 * 7919 + 11) * std::f32::consts::TAU;
        let brightness = super::quantize(lamp_brightness(
            age_secs,
            entry.running,
            sig.sticky_error,
            sig.flash_intensity(),
            t,
            phase,
        ));
        let hue = lamp_hue(sig.sticky_error, sig.flash_intensity(), warm, FLASH_HUE, ERROR_HUE);
        let target = Vec3::new(hue[0], hue[1], hue[2]) * brightness;
        super::set_glow(&mut mats, &handle.0, target);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn id_of(n: u8) -> ContextId {
        let mut b = [0u8; 16];
        b[0] = n;
        ContextId::from_bytes(b)
    }

    /// Build a `ContextInfo` with the fields the switchboard cares about;
    /// the rest default (mirrors `time_well::card`'s own test fixture).
    fn ctx(n: u8, last_activity_at: Option<u64>, created_at: u64, archived: bool) -> ContextInfo {
        ContextInfo {
            id: id_of(n),
            label: format!("ctx{n}"),
            forked_from: None,
            provider: String::new(),
            model: String::new(),
            created_at,
            trace_id: [0u8; 16],
            fork_kind: None,
            context_type: String::new(),
            archived,
            concluded_at: None,
            keywords: Vec::new(),
            top_block_preview: None,
            live_status: Status::Pending,
            last_activity_at,
            track_id: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
            context_window: None,
            context_used_tokens: None,
            context_used_pct: None,
            background_running_count: 0,
            background_oldest_running_started_at: None,
            background_last_finished_at: None,
            background_last_finished_status: None,
            background_last_exit_code: None,
            cast_label: None,
        }
    }

    // ── recency_brightness ──

    #[test]
    fn recency_brightness_is_maximal_at_zero_age() {
        assert_eq!(recency_brightness(0.0), 1.0);
    }

    #[test]
    fn recency_brightness_decays_smoothly_and_monotonically() {
        let samples: Vec<f32> = (0..20).map(|i| recency_brightness(i as f32 * 300.0)).collect();
        for w in samples.windows(2) {
            assert!(w[0] >= w[1], "brightness must never increase with age: {samples:?}");
        }
    }

    #[test]
    fn recency_brightness_lands_roughly_in_the_mission_bands() {
        assert!(recency_brightness(60.0) > 0.85, "under a minute should still read bright");
        let ten_min = recency_brightness(600.0);
        assert!((0.3..0.75).contains(&ten_min), "ten minutes should read as a mid warm value: {ten_min}");
        assert!(recency_brightness(3600.0) < 0.15, "an hour should be down near the floor");
    }

    #[test]
    fn recency_brightness_asymptotes_to_the_floor_never_reaching_zero() {
        let very_old = recency_brightness(1.0e9);
        assert!((very_old - RECENCY_FLOOR).abs() < 1e-3);
        assert!(very_old > 0.0, "a lamp with a context behind it never goes fully dark");
    }

    #[test]
    fn recency_brightness_rejects_negative_age_by_clamping() {
        assert_eq!(recency_brightness(-50.0), recency_brightness(0.0));
    }

    // ── select_lamp_contexts ──

    #[test]
    fn select_lamp_contexts_orders_most_recent_first() {
        let contexts = vec![
            ctx(1, Some(100), 0, false),
            ctx(2, Some(300), 0, false),
            ctx(3, Some(200), 0, false),
        ];
        let order = select_lamp_contexts(&contexts, 24);
        assert_eq!(order, vec![id_of(2), id_of(3), id_of(1)]);
    }

    #[test]
    fn select_lamp_contexts_falls_back_to_created_at_with_no_last_activity() {
        let contexts = vec![ctx(1, None, 500, false), ctx(2, Some(100), 0, false)];
        let order = select_lamp_contexts(&contexts, 24);
        assert_eq!(order, vec![id_of(1), id_of(2)], "created_at stands in for a never-touched context");
    }

    #[test]
    fn select_lamp_contexts_excludes_archived() {
        let contexts = vec![ctx(1, Some(500), 0, true), ctx(2, Some(100), 0, false)];
        let order = select_lamp_contexts(&contexts, 24);
        assert_eq!(order, vec![id_of(2)], "an archived context never gets a lamp, however recent");
    }

    #[test]
    fn select_lamp_contexts_caps_at_the_given_limit() {
        let contexts: Vec<ContextInfo> =
            (0..30u8).map(|n| ctx(n, Some(n as u64), 0, false)).collect();
        let order = select_lamp_contexts(&contexts, LAMP_CAP);
        assert_eq!(order.len(), LAMP_CAP);
        // Highest last_activity_at values win (29 down to 6).
        assert_eq!(order[0], id_of(29));
        assert_eq!(order[LAMP_CAP - 1], id_of(30 - LAMP_CAP as u8));
    }

    #[test]
    fn select_lamp_contexts_breaks_ties_deterministically_on_id() {
        let a = vec![ctx(5, Some(100), 0, false), ctx(2, Some(100), 0, false)];
        let b = vec![ctx(2, Some(100), 0, false), ctx(5, Some(100), 0, false)];
        // Same set, different input order — must produce the same output
        // order (id-ascending tiebreak), not whatever order they arrived in.
        assert_eq!(select_lamp_contexts(&a, 24), select_lamp_contexts(&b, 24));
        assert_eq!(select_lamp_contexts(&a, 24), vec![id_of(2), id_of(5)]);
    }

    // ── lamp_grid_local_xy ──

    #[test]
    fn grid_slot_zero_is_top_left() {
        let (x, y) = lamp_grid_local_xy(0, 40.0, 10.0);
        assert!(x < 0.0, "leftmost column: {x}");
        assert!(y > 0.0, "top row: {y}");
    }

    #[test]
    fn grid_last_slot_is_bottom_right() {
        let (x, y) = lamp_grid_local_xy(LAMP_CAP - 1, 40.0, 10.0);
        assert!(x > 0.0, "rightmost column: {x}");
        assert!(y < 0.0, "bottom row: {y}");
    }

    #[test]
    fn grid_row_shares_the_same_y() {
        let (_, y0) = lamp_grid_local_xy(0, 40.0, 10.0);
        let (_, y_last_in_row) = lamp_grid_local_xy(LAMP_GRID_COLS - 1, 40.0, 10.0);
        assert_eq!(y0, y_last_in_row, "the whole top row sits at one height");
    }

    #[test]
    fn grid_is_centered_on_the_panel() {
        let cell = 40.0;
        let gap = 10.0;
        let sum_x: f32 = (0..LAMP_CAP).map(|i| lamp_grid_local_xy(i, cell, gap).0).sum();
        let sum_y: f32 = (0..LAMP_CAP).map(|i| lamp_grid_local_xy(i, cell, gap).1).sum();
        assert!(sum_x.abs() < 1e-3, "columns balance around x=0: {sum_x}");
        assert!(sum_y.abs() < 1e-3, "rows balance around y=0: {sum_y}");
    }

    #[test]
    fn grid_slots_never_overlap() {
        let cell = 40.0;
        let gap = 10.0;
        let positions: Vec<(f32, f32)> = (0..LAMP_CAP).map(|i| lamp_grid_local_xy(i, cell, gap)).collect();
        for i in 0..positions.len() {
            for j in (i + 1)..positions.len() {
                let (x0, y0) = positions[i];
                let (x1, y1) = positions[j];
                assert!(
                    (x0 - x1).abs() >= cell || (y0 - y1).abs() >= cell,
                    "slots {i} and {j} overlap: {positions:?}"
                );
            }
        }
    }

    // ── LampSignal ──

    #[test]
    fn turn_failed_sets_a_sticky_error() {
        let mut sig = LampSignal::default();
        assert!(!sig.sticky_error);
        sig.on_turn_failed();
        assert!(sig.sticky_error);
        // Time alone must not clear it.
        for _ in 0..1000 {
            sig.tick(1.0);
        }
        assert!(sig.sticky_error, "an ember is sticky — it does not decay with time");
    }

    #[test]
    fn turn_completed_clears_a_sticky_error_and_starts_a_flash() {
        let mut sig = LampSignal::default();
        sig.on_turn_failed();
        assert!(sig.sticky_error);
        sig.on_turn_completed();
        assert!(!sig.sticky_error, "fresh successful activity clears the ember");
        assert_eq!(sig.flash_intensity(), 1.0, "the flash starts at full intensity");
    }

    #[test]
    fn reconcile_status_error_sets_and_running_or_pending_clears() {
        let mut sig = LampSignal::default();
        sig.reconcile_status(Status::Error);
        assert!(sig.sticky_error, "a polled Error status sets the ember too, not only TurnFailed");
        sig.reconcile_status(Status::Running);
        assert!(!sig.sticky_error, "a poll showing Running clears it");

        sig.reconcile_status(Status::Error);
        sig.reconcile_status(Status::Pending);
        assert!(!sig.sticky_error, "a poll showing Pending clears it too");

        sig.reconcile_status(Status::Error);
        sig.reconcile_status(Status::Done);
        assert!(!sig.sticky_error, "Done also counts as fresh successful activity");
    }

    #[test]
    fn on_turn_failed_cancels_any_flash_in_flight() {
        let mut sig = LampSignal::default();
        sig.on_turn_completed();
        assert!(sig.flash_intensity() > 0.0);
        sig.on_turn_failed();
        assert_eq!(sig.flash_intensity(), 0.0, "a failure must not let a flash paint over the ember");
    }

    #[test]
    fn flash_decays_to_zero_over_its_duration_and_never_goes_negative() {
        let mut sig = LampSignal::default();
        sig.on_turn_completed();
        let mut last = sig.flash_intensity();
        for _ in 0..200 {
            sig.tick(0.05);
            let now = sig.flash_intensity();
            assert!(now <= last + 1e-6, "flash intensity must never increase mid-decay");
            assert!(now >= 0.0);
            last = now;
        }
        assert_eq!(sig.flash_intensity(), 0.0, "long past FLASH_DURATION_SECS the flash is gone");
    }

    #[test]
    fn is_active_tracks_sticky_error_and_flash_independently() {
        let mut sig = LampSignal::default();
        assert!(!sig.is_active());
        sig.on_turn_failed();
        assert!(sig.is_active());
        sig.reconcile_status(Status::Running);
        assert!(!sig.is_active());
        sig.on_turn_completed();
        assert!(sig.is_active(), "an in-flight flash counts as active too");
        for _ in 0..200 {
            sig.tick(0.05);
        }
        assert!(!sig.is_active());
    }

    // ── lamp_brightness / lamp_hue combination ──

    #[test]
    fn sticky_error_overrides_recency_and_breathing() {
        // A very old, non-running context that's nonetheless erroring must
        // still read at the fixed error brightness, not a dim recency value.
        let b = lamp_brightness(1.0e6, false, true, 0.0, 0.0, 0.0);
        assert_eq!(b, ERROR_BRIGHTNESS);
    }

    #[test]
    fn flash_adds_on_top_of_the_base_brightness() {
        let base = lamp_brightness(0.0, false, false, 0.0, 0.0, 0.0);
        let flashed = lamp_brightness(0.0, false, false, 1.0, 0.0, 0.0);
        assert!(flashed > base, "a full-intensity flash must brighten the lamp");
    }

    #[test]
    fn running_breathing_stays_within_its_band() {
        // Sweep t across a full cycle; brightness should oscillate but never
        // dip below the recency base scaled by the trough, nor exceed it.
        let age = 0.0; // brightest recency baseline
        let base = recency_brightness(age);
        for i in 0..40 {
            let t = i as f32 * 0.25;
            let b = lamp_brightness(age, true, false, 0.0, t, 0.0);
            assert!(b <= base + 1e-4, "breathing must not exceed the recency base: {b} > {base}");
            assert!(b >= base * BREATH_TROUGH - 1e-4, "breathing must not dip below its trough: {b}");
        }
    }

    #[test]
    fn error_hue_is_used_regardless_of_flash_intensity() {
        let warm = [1.0, 0.78, 0.34];
        let hue = lamp_hue(true, 1.0, warm, FLASH_HUE, ERROR_HUE);
        assert_eq!(hue, ERROR_HUE);
    }

    #[test]
    fn flash_hue_blends_from_warm_toward_flash_at_full_intensity() {
        let warm = [1.0, 0.78, 0.34];
        let none = lamp_hue(false, 0.0, warm, FLASH_HUE, ERROR_HUE);
        let full = lamp_hue(false, 1.0, warm, FLASH_HUE, ERROR_HUE);
        assert_eq!(none, warm, "no flash — pure warm base");
        assert_eq!(full, FLASH_HUE, "full flash — pure flash hue");
    }

    #[test]
    fn breathing_multiplier_stays_within_its_trough_to_one_band() {
        for i in 0..50 {
            let t = i as f32 * 0.37;
            let m = breathing_multiplier(t, 0.0);
            assert!(
                (BREATH_TROUGH - 1e-4..=1.0 + 1e-4).contains(&m),
                "breathing_multiplier out of band at t={t}: {m}"
            );
        }
    }
}
