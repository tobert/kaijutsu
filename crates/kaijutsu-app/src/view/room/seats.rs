//! Seats at the table: one small glowing "wisp" per attached peer
//! (`ActorHandle::list_peers` — every connected client, the Bevy app
//! included, attaches with a `<kind>/<name>` nick), drifting in a slow orbit
//! around the well at the center of the room. The instrument metaphor
//! (`CLAUDE.md`'s "many hands, one keyboard"): every wisp is a hand on the
//! instrument, not a UI widget — a firefly/familiar read, not a nameplate
//! (mission: "at room scale they'd be illegible clutter" — a LOD-gated label
//! on well-zoom is a follow-up, `docs/issues.md`).
//!
//! **Design split** (same stance as [`super::switchboard`]'s own header):
//! hue derivation, home-orbit placement, the orbit+wander position curve,
//! and the arrive/depart fade state machine are pure — no Bevy types,
//! unit-tested below. Only the spawn/reconcile/sync systems at the bottom
//! touch `bevy` or [`kaijutsu_client::PeerInfo`].
//!
//! **Transform mutation is fine every frame; material writes are not**
//! (this room's settled-material discipline, `super`'s module doc + the
//! well's `LIVE_LANE_STEP`). [`wisp_position`] is a pure function of
//! `(nick, elapsed_secs)` re-evaluated fresh each frame in [`sync_seats`] —
//! cheap, no stored per-frame state. The material only changes while a
//! wisp's fade is in flight ([`WispFade::opacity`] moving); once a wisp has
//! fully arrived and its fade has settled, [`super::quantize`]/
//! [`super::set_glow`] stop touching `Assets<StandardMaterial>` for it, same
//! as an idle switchboard lamp.
//!
//! **Nick-keying is a known wire limitation** (docs/issues.md, "Peer-registry
//! doctrine" entry): `PeerInfo` carries only `nick`/`attached_at`, no
//! `instance` — two peers sharing a nick (two app windows, two MCP sessions
//! under the same label) collapse onto one seat today. Not fought here.

use std::collections::{HashMap, HashSet};
use std::f32::consts::TAU;

use bevy::prelude::*;

use super::bearing;
use crate::connection::peers::PeerRoster;

// ── Tunables (Amy-tunable — first guess, live-tune over BRP) ────────────────

/// Wisp mesh diameter (world units) — "small ~12-20 world units across"
/// (mission), a icosphere radius half that.
const WISP_RADIUS: f32 = 8.0;

/// "Table airspace" orbit band: between the well ring (radius 230) and the
/// table edge, so wisps read as *at* the table, not off in the room. Home
/// radius for a given nick is drawn once from this band ([`nick_home`]); the
/// wander wobble ([`WANDER_XZ_AMPLITUDE`]) can push a little outside it —
/// tests account for that.
const ORBIT_RADIUS_MIN: f32 = 300.0;
const ORBIT_RADIUS_MAX: f32 = 450.0;

/// Resting height band — above the floor, below wall content (mission).
const SEAT_Y_MIN: f32 = 150.0;
const SEAT_Y_MAX: f32 = 260.0;

/// Slow orbital progression, radians/sec — direction (cw/ccw) drawn
/// independently per nick ([`nick_motion`]) so the roster doesn't all drift
/// the same way. At [`ORBIT_RATE_MAX`] a full lap takes ~3 minutes; this is
/// ambient motion, not a readout.
const ORBIT_RATE_MIN: f32 = 0.015;
const ORBIT_RATE_MAX: f32 = 0.035;

/// Small sine-sum wander on top of the orbit — "alive, not mechanical"
/// (mission) — amplitude in world units and per-nick rate band (seconds⁻¹,
/// i.e. angular frequency in rad/s of the sine argument).
const WANDER_XZ_AMPLITUDE: f32 = 18.0;
const WANDER_Y_AMPLITUDE: f32 = 14.0;
const WANDER_RATE_MIN: f32 = 0.15;
const WANDER_RATE_MAX: f32 = 0.35;

/// Arrive/depart fade timing (mission: "~2s" in, "~3s" out).
const ARRIVE_DURATION_SECS: f32 = 2.0;
const DEPART_DURATION_SECS: f32 = 3.0;

/// Identity hue saturation/lightness — the room's palette family, same
/// `Color::hsl` idiom `time_well::scene::accent_color` uses for its own
/// hash-to-hue placeholder, tuned a little more saturated/bright since this
/// hue drives an HDR emissive rather than a card fill.
const SEAT_SATURATION: f32 = 0.62;
const SEAT_LIGHTNESS: f32 = 0.55;
/// Brightness multiplier at full presence (`opacity() == 1.0`) — > 1.0 so a
/// settled wisp blooms through the HDR camera (room convention, `super`'s
/// module doc).
const SEAT_BRIGHTNESS: f32 = 1.7;

// ── Pure: deterministic per-nick hash draws ──────────────────────────────────

/// FNV-1a over a nick's bytes — the same dependency-free hash
/// `time_well::scene::accent_color` uses for its own hash-to-hue.
fn fnv1a(bytes: &[u8]) -> u32 {
    let mut h: u32 = 2166136261;
    for &b in bytes {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    h
}

/// A per-nick unit float in `[0, 1)`, one independent draw per `salt` —
/// folds the nick's FNV hash with `salt` and remixes through
/// [`bearing::hash01`] (its own doc: mantissa-safe, half-open) so a wisp's
/// hue, home angle, orbit radius/rate/direction, and wander phases/rate
/// don't covary: tweaking what salt N means never reshuffles salt M's draw.
fn nick_draw(nick: &str, salt: u32) -> f32 {
    let base = fnv1a(nick.as_bytes());
    bearing::hash01(base.wrapping_add(salt.wrapping_mul(0x9E37_79B1)))
}

/// Deterministic hue in degrees `[0, 360)` for a peer's nick — same nick,
/// same hue, every session (mission: "Amy learns 'that teal one is
/// mcp/toad'"). The raw draw is pushed through one more golden-ratio
/// fractional multiply before mapping to degrees: nicks whose FNV bytes
/// happen to land close together (e.g. "mcp/toad" vs "mcp/toat") would
/// otherwise often hash to adjacent, hard-to-distinguish hues; multiplying
/// by an irrational constant and re-fracting is the standard "N distinct
/// colors" decorrelation step — small input differences become large output
/// differences.
pub fn nick_hue_degrees(nick: &str) -> f32 {
    const GOLDEN_RATIO: f32 = 1.618_034;
    let draw = nick_draw(nick, 0);
    (draw * GOLDEN_RATIO).fract() * 360.0
}

/// The identity linear-RGB color for a nick's wisp — [`nick_hue_degrees`] at
/// a fixed saturation/lightness ([`SEAT_SATURATION`]/[`SEAT_LIGHTNESS`]),
/// through `Color::hsl` (CSS-HSL, sRGB-referenced) → `to_linear()`, the
/// scene's color-space contract (`scene_palette`'s module doc: "everything
/// in this resource is linear").
pub fn nick_identity_color(nick: &str) -> Vec3 {
    let hue = nick_hue_degrees(nick);
    let c = Color::hsl(hue, SEAT_SATURATION, SEAT_LIGHTNESS).to_linear();
    Vec3::new(c.red, c.green, c.blue)
}

/// A nick's home orbit: `(angle radians in [0, TAU), radius in
/// [ORBIT_RADIUS_MIN, ORBIT_RADIUS_MAX], resting height in [SEAT_Y_MIN,
/// SEAT_Y_MAX])`. Each drawn from an independent salt.
pub fn nick_home(nick: &str) -> (f32, f32, f32) {
    let angle = nick_draw(nick, 1) * TAU;
    let radius = ORBIT_RADIUS_MIN + nick_draw(nick, 2) * (ORBIT_RADIUS_MAX - ORBIT_RADIUS_MIN);
    let y = SEAT_Y_MIN + nick_draw(nick, 3) * (SEAT_Y_MAX - SEAT_Y_MIN);
    (angle, radius, y)
}

/// A nick's orbital rate (rad/s, signed — direction drawn independently) and
/// wander rate/phases. Kept separate from [`nick_home`] (different salts) so
/// this module can change the wander model without touching where a wisp's
/// orbit is centered.
fn nick_motion(nick: &str) -> (f32, f32, f32, f32, f32) {
    let dir = if nick_draw(nick, 4) < 0.5 { -1.0 } else { 1.0 };
    let orbit_rate = dir * (ORBIT_RATE_MIN + nick_draw(nick, 5) * (ORBIT_RATE_MAX - ORBIT_RATE_MIN));
    let wander_rate = WANDER_RATE_MIN + nick_draw(nick, 6) * (WANDER_RATE_MAX - WANDER_RATE_MIN);
    let phase_x = nick_draw(nick, 7) * TAU;
    let phase_y = nick_draw(nick, 8) * TAU;
    let phase_z = nick_draw(nick, 9) * TAU;
    (orbit_rate, wander_rate, phase_x, phase_y, phase_z)
}

/// World-space position of a nick's wisp at elapsed room time `t` (seconds):
/// orbit progression around the well plus a small phase-offset sine-sum
/// wander, so a full roster doesn't move in lockstep. Pure function of
/// `(nick, t)` — no stored per-frame state; [`sync_seats`] just evaluates it
/// fresh every frame ([`sync_seats`]'s own doc has why that's fine here even
/// though material writes are budgeted).
pub fn wisp_position(nick: &str, t: f32) -> Vec3 {
    let (angle0, radius, y0) = nick_home(nick);
    let (orbit_rate, wander_rate, phase_x, phase_y, phase_z) = nick_motion(nick);
    let angle = angle0 + orbit_rate * t;
    let wx = WANDER_XZ_AMPLITUDE * (t * wander_rate + phase_x).sin();
    let wz = WANDER_XZ_AMPLITUDE * (t * wander_rate * 1.3 + phase_z).sin();
    let wy = WANDER_Y_AMPLITUDE * (t * wander_rate * 0.7 + phase_y).sin();
    Vec3::new(angle.cos() * radius + wx, y0 + wy, angle.sin() * radius + wz)
}

// ── Pure: arrive/depart fade ──────────────────────────────────────────────────

/// Arrive/depart fade-and-scale state for one wisp — a continuous opacity in
/// `[0, 1]` chasing a target at a fixed rate (rising at
/// `1 / ARRIVE_DURATION_SECS`, falling at `1 / DEPART_DURATION_SECS`), NOT a
/// two-state machine with a fixed-length countdown: a peer that re-appears
/// mid-fade-out reverses smoothly from wherever its opacity currently sits
/// (no pop — [`WispFade::begin_arrive`] just flips the target), and a peer
/// that departs mid-arrival fades out proportionally faster (it had less
/// brightness to lose) rather than restarting a fixed 3s countdown from
/// full.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WispFade {
    opacity: f32,
    target: f32,
}

impl WispFade {
    /// A brand-new wisp: invisible, ramping toward full presence.
    pub fn arriving() -> Self {
        Self { opacity: 0.0, target: 1.0 }
    }

    /// The peer left the roster — start fading toward gone.
    pub fn begin_depart(&mut self) {
        self.target = 0.0;
    }

    /// The peer is (still) live — (re)approach full presence. Safe to call
    /// every reconcile pass for a persisting nick: a no-op once already at
    /// `target == 1.0`, and the recovery path for a nick that re-appeared
    /// while its previous wisp was mid-fade-out.
    pub fn begin_arrive(&mut self) {
        self.target = 1.0;
    }

    /// Advance `dt` seconds toward the current target.
    pub fn tick(&mut self, dt: f32) {
        let rate =
            if self.target >= self.opacity { 1.0 / ARRIVE_DURATION_SECS } else { 1.0 / DEPART_DURATION_SECS };
        let delta = rate * dt;
        if self.opacity < self.target {
            self.opacity = (self.opacity + delta).min(self.target);
        } else if self.opacity > self.target {
            self.opacity = (self.opacity - delta).max(self.target);
        }
    }

    /// Current visible fraction — drives brightness and scale together
    /// ("fades+scales in", mission).
    pub fn opacity(&self) -> f32 {
        self.opacity
    }

    /// Faded fully to nothing with nowhere further to go — safe to despawn.
    /// An epsilon rather than an exact `== 0.0`: real frame `dt`s (and the
    /// fixed-step test sweeps below) don't divide [`DEPART_DURATION_SECS`]
    /// evenly, so the last tick can land a hair above zero by floating-point
    /// drift alone — visually indistinguishable from gone, and worth
    /// despawning a frame early rather than holding a dust-mote-opacity
    /// entity forever.
    pub fn is_despawnable(&self) -> bool {
        const GONE_EPSILON: f32 = 1e-3;
        self.target <= 0.0 && self.opacity <= GONE_EPSILON
    }
}

// ── Pure: roster reconcile ───────────────────────────────────────────────────

/// The three-way split of a roster poll against the wisps already known:
/// nicks newly live (spawn), nicks known but no longer live (mark
/// departing), and nicks live in both (persisting — recovers a mid-depart
/// wisp). Pure set arithmetic so [`reconcile_seats`] stays a thin
/// spawn/mark loop.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RosterDelta {
    pub appeared: Vec<String>,
    pub departed: Vec<String>,
    pub persisted: Vec<String>,
}

pub fn classify_roster(known: &HashSet<String>, live: &HashSet<String>) -> RosterDelta {
    RosterDelta {
        appeared: live.difference(known).cloned().collect(),
        departed: known.difference(live).cloned().collect(),
        persisted: known.intersection(live).cloned().collect(),
    }
}

// ── Bevy glue ─────────────────────────────────────────────────────────────────

/// One wisp entity — its peer nick, so [`sync_seats`] can re-derive hue and
/// position without duplicating them into the component.
#[derive(Component)]
pub(crate) struct Wisp {
    nick: String,
}

/// Runtime bookkeeping for one seated (or fading-out) nick.
struct WispRuntime {
    entity: Entity,
    fade: WispFade,
}

/// Live wisp roster, keyed by nick — see this module's header doc for the
/// nick-keying limitation. `pub(crate)` so `super::exit_room`'s teardown can
/// clear it (`clear_seats`).
#[derive(Resource, Default)]
pub(crate) struct SeatsState {
    wisps: HashMap<String, WispRuntime>,
}

/// Drop all wisp bookkeeping on the way out of the room. The entities
/// themselves already died with `RoomRoot`'s recursive despawn
/// (`super::teardown_room`) — without this, a stale nick→`Entity` pair would
/// survive to the next room visit and `reconcile_seats` would read it as
/// "already seated," silently leaving that peer with no wisp (mission:
/// "re-entering the room respawns from the roster").
pub(crate) fn clear_seats(mut state: ResMut<SeatsState>) {
    state.wisps.clear();
}

/// Spawn a wisp for a newly appeared nick, mark a wisp departing when its
/// nick drops off the roster, and recover a persisting nick's fade toward
/// full presence — the spawn/mark half of the seat lifecycle ([`sync_seats`]
/// is the per-frame tick/motion/material/despawn half). Runs every frame
/// while in the room (not gated on `PeerRoster` change-detection): the
/// roster is small, the diff is cheap, and un-gating it is what lets a fresh
/// room entry — `SeatsState` just cleared by [`clear_seats`] — spawn every
/// still-attached peer immediately rather than waiting for the next 5s poll
/// to happen to land after this visit started. Gated to `Screen::Room`
/// (`RoomPlugin`'s registration): a wisp needs `RoomRoot` to parent onto,
/// which only exists while the room scene is alive.
pub(crate) fn reconcile_seats(
    mut commands: Commands,
    roster: Res<PeerRoster>,
    mut state: ResMut<SeatsState>,
    root_query: Query<Entity, With<super::RoomRoot>>,
    time: Res<Time>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut mats: ResMut<Assets<StandardMaterial>>,
) {
    let Ok(root) = root_query.single() else { return };

    let known: HashSet<String> = state.wisps.keys().cloned().collect();
    let live: HashSet<String> = roster.peers.iter().map(|p| p.nick.clone()).collect();
    let delta = classify_roster(&known, &live);
    if delta.appeared.is_empty() && delta.departed.is_empty() && delta.persisted.is_empty() {
        return;
    }

    let t = time.elapsed_secs();
    // Only allocated when there's actually something to spawn this frame —
    // `persisted` (every unchanged nick) is non-empty on almost every
    // steady-state frame once any peer is attached, so an unconditional
    // `meshes.add` here would allocate+immediately-drop a sphere mesh every
    // single frame for no reason (kaibo review, 2026-08-10).
    let mesh = if delta.appeared.is_empty() { None } else { Some(meshes.add(Sphere::new(WISP_RADIUS))) };

    for nick in delta.appeared {
        // Starts fully transparent/black (`opacity() == 0.0` at spawn) —
        // `sync_seats` paints the first real color/scale the very next
        // frame; no visible pop at spawn.
        let mat = mats.add(super::unlit(super::lin_v(Vec3::ZERO)));
        let entity = commands
            .spawn((
                Wisp { nick: nick.clone() },
                super::RoomDistraction,
                Mesh3d(mesh.clone().expect("mesh is Some whenever `appeared` is non-empty")),
                MeshMaterial3d(mat),
                Transform::from_translation(wisp_position(&nick, t)).with_scale(Vec3::ZERO),
                Visibility::Inherited,
                Name::new(format!("Seat-{nick}")),
                ChildOf(root),
            ))
            .id();
        state.wisps.insert(nick, WispRuntime { entity, fade: WispFade::arriving() });
    }

    for nick in delta.departed {
        if let Some(w) = state.wisps.get_mut(&nick) {
            w.fade.begin_depart();
        }
    }

    for nick in delta.persisted {
        if let Some(w) = state.wisps.get_mut(&nick) {
            w.fade.begin_arrive();
        }
    }
}

/// Tick every wisp's fade, move it along its orbit, write brightness/scale
/// from `opacity()`, and despawn a departing wisp once its fade finishes
/// (`WispFade::is_despawnable`). `Transform` writes happen unconditionally
/// every frame (cheap, not a `Handle<Asset>` write — this room's material
/// budget rule doesn't apply to it, `super`'s module doc); `base_color`
/// writes go through [`super::set_glow`]'s epsilon-gated compare, so a
/// settled wisp (opacity holding at 0 or 1, position still orbiting) stops
/// touching `Assets<StandardMaterial>` even though its `Transform` keeps
/// moving. Gated to `Screen::Room`, `.after(reconcile_seats)` so a wisp
/// spawned this frame is ticked/moved the same frame (`RoomPlugin`'s
/// registration).
pub(crate) fn sync_seats(
    mut commands: Commands,
    time: Res<Time>,
    mut state: ResMut<SeatsState>,
    mut mats: ResMut<Assets<StandardMaterial>>,
    mut wisps: Query<(&Wisp, &mut Transform, &MeshMaterial3d<StandardMaterial>)>,
) {
    let dt = time.delta_secs();
    let t = time.elapsed_secs();

    let mut finished = Vec::new();
    for (nick, runtime) in state.wisps.iter_mut() {
        runtime.fade.tick(dt);
        if runtime.fade.is_despawnable() {
            commands.entity(runtime.entity).despawn();
            finished.push(nick.clone());
        }
    }
    for nick in finished {
        state.wisps.remove(&nick);
    }

    for (wisp, mut tf, handle) in wisps.iter_mut() {
        // A wisp whose entry was just removed above (despawn commands
        // haven't applied yet this frame) is skipped rather than painted —
        // no point writing a material about to disappear.
        let Some(runtime) = state.wisps.get(&wisp.nick) else { continue };
        let opacity = runtime.fade.opacity();
        tf.translation = wisp_position(&wisp.nick, t);
        tf.scale = Vec3::splat(opacity);
        let brightness = super::quantize(SEAT_BRIGHTNESS * opacity);
        let target = nick_identity_color(&wisp.nick) * brightness;
        super::set_glow(&mut mats, &handle.0, target);
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── nick_hue_degrees ──

    #[test]
    fn hue_is_deterministic_for_the_same_nick() {
        assert_eq!(nick_hue_degrees("mcp/toad"), nick_hue_degrees("mcp/toad"));
    }

    #[test]
    fn hue_stays_in_degree_range() {
        for nick in ["mcp/toad", "kaijutsu-app", "mcp/toat", "acp/claude", ""] {
            let hue = nick_hue_degrees(nick);
            assert!((0.0..360.0).contains(&hue), "{nick} hue out of range: {hue}");
        }
    }

    #[test]
    fn distinct_nicks_usually_land_on_distinct_hues() {
        let nicks = [
            "mcp/toad", "kaijutsu-app", "mcp/fern", "acp/claude", "mcp/newt", "mcp/moth", "mcp/wisp",
        ];
        let hues: Vec<f32> = nicks.iter().map(|n| nick_hue_degrees(n)).collect();
        for i in 0..hues.len() {
            for j in (i + 1)..hues.len() {
                assert!(
                    (hues[i] - hues[j]).abs() > 1.0,
                    "{} and {} collided on hue {}",
                    nicks[i],
                    nicks[j],
                    hues[i]
                );
            }
        }
    }

    #[test]
    fn similar_nicks_do_not_hash_to_adjacent_hues() {
        // The whole point of the golden-angle decorrelation step: two nicks
        // one byte apart must not produce two hues a few degrees apart.
        let a = nick_hue_degrees("mcp/toad");
        let b = nick_hue_degrees("mcp/toae");
        let diff = (a - b).abs();
        let wrapped = diff.min(360.0 - diff);
        assert!(wrapped > 5.0, "adjacent nicks landed suspiciously close: {a} vs {b}");
    }

    // ── nick_home ──

    #[test]
    fn home_angle_radius_and_height_stay_in_band() {
        for nick in ["mcp/toad", "kaijutsu-app", "acp/claude", "mcp/x"] {
            let (angle, radius, y) = nick_home(nick);
            assert!((0.0..TAU).contains(&angle), "{nick} angle out of range: {angle}");
            assert!(
                (ORBIT_RADIUS_MIN..=ORBIT_RADIUS_MAX).contains(&radius),
                "{nick} home radius out of band: {radius}"
            );
            assert!((SEAT_Y_MIN..=SEAT_Y_MAX).contains(&y), "{nick} home height out of band: {y}");
        }
    }

    #[test]
    fn home_is_deterministic() {
        assert_eq!(nick_home("mcp/toad"), nick_home("mcp/toad"));
    }

    // ── wisp_position ──

    #[test]
    fn position_stays_within_radius_and_height_band_over_a_time_sweep() {
        // The XZ wander is two independent sine terms (wx, wz), each bounded
        // by the amplitude but at different phase/frequency — their combined
        // vector can reach sqrt(2)*amplitude in the worst-case direction, not
        // just amplitude (triangle inequality on `|R_vec + W_vec|`).
        let wander_margin = WANDER_XZ_AMPLITUDE * std::f32::consts::SQRT_2;
        let max_horizontal = ORBIT_RADIUS_MAX + wander_margin + 1e-3;
        let min_horizontal = (ORBIT_RADIUS_MIN - wander_margin).max(0.0) - 1e-3;
        let max_y = SEAT_Y_MAX + WANDER_Y_AMPLITUDE + 1e-3;
        let min_y = SEAT_Y_MIN - WANDER_Y_AMPLITUDE - 1e-3;

        for nick in ["mcp/toad", "kaijutsu-app", "acp/claude"] {
            for i in 0..200 {
                let t = i as f32 * 3.7; // sweep well past any orbit period
                let pos = wisp_position(nick, t);
                let horizontal = (pos.x * pos.x + pos.z * pos.z).sqrt();
                assert!(
                    horizontal <= max_horizontal && horizontal >= min_horizontal,
                    "{nick}@t={t}: horizontal {horizontal} out of band [{min_horizontal}, {max_horizontal}]"
                );
                assert!(
                    pos.y <= max_y && pos.y >= min_y,
                    "{nick}@t={t}: y {} out of band [{min_y}, {max_y}]",
                    pos.y
                );
            }
        }
    }

    #[test]
    fn position_is_a_pure_function_of_nick_and_time() {
        let a = wisp_position("mcp/toad", 12.5);
        let b = wisp_position("mcp/toad", 12.5);
        assert_eq!(a, b);
    }

    #[test]
    fn different_nicks_at_the_same_time_usually_differ() {
        let a = wisp_position("mcp/toad", 0.0);
        let b = wisp_position("kaijutsu-app", 0.0);
        assert_ne!(a, b);
    }

    // ── WispFade ──

    #[test]
    fn arriving_ramps_zero_to_one_over_arrive_duration() {
        let mut fade = WispFade::arriving();
        assert_eq!(fade.opacity(), 0.0);
        let steps = 40;
        let dt = ARRIVE_DURATION_SECS / steps as f32;
        let mut last = fade.opacity();
        for _ in 0..steps {
            fade.tick(dt);
            let now = fade.opacity();
            assert!(now >= last - 1e-6, "opacity must not decrease while arriving: {last} -> {now}");
            last = now;
        }
        assert!((fade.opacity() - 1.0).abs() < 1e-4, "should have reached full presence: {}", fade.opacity());
    }

    #[test]
    fn departing_ramps_to_zero_then_reports_despawnable() {
        let mut fade = WispFade::arriving();
        fade.tick(ARRIVE_DURATION_SECS); // fully arrived
        assert_eq!(fade.opacity(), 1.0);
        assert!(!fade.is_despawnable());

        fade.begin_depart();
        assert!(!fade.is_despawnable(), "must not despawn the instant depart starts");

        let steps = 60;
        let dt = DEPART_DURATION_SECS / steps as f32;
        let mut last = fade.opacity();
        for _ in 0..steps {
            fade.tick(dt);
            let now = fade.opacity();
            assert!(now <= last + 1e-6, "opacity must not increase while departing: {last} -> {now}");
            last = now;
        }
        assert!(fade.opacity() < 1e-3, "should have faded down to (near) zero: {}", fade.opacity());
        assert!(fade.is_despawnable(), "fully faded out with no target to move toward — safe to despawn");
    }

    #[test]
    fn reappearing_mid_fade_out_recovers_without_a_pop() {
        let mut fade = WispFade::arriving();
        fade.tick(ARRIVE_DURATION_SECS);
        fade.begin_depart();
        fade.tick(DEPART_DURATION_SECS * 0.5); // now partway faded out
        let mid_opacity = fade.opacity();
        assert!(mid_opacity > 0.0 && mid_opacity < 1.0);

        // The peer is back — recover from exactly where the fade-out left
        // off, no snap to zero or pop back to full.
        fade.begin_arrive();
        assert_eq!(fade.opacity(), mid_opacity, "recovering must not itself move opacity");
        assert!(!fade.is_despawnable());

        fade.tick(0.01);
        assert!(fade.opacity() > mid_opacity, "ticking after recovery must move opacity back up");

        fade.tick(ARRIVE_DURATION_SECS * 2.0); // plenty of time to fully recover
        assert!((fade.opacity() - 1.0).abs() < 1e-4, "should reach full presence again: {}", fade.opacity());
    }

    #[test]
    fn departing_mid_arrival_is_not_a_hard_reset() {
        // A peer that departs before finishing its arrival fade must not
        // suddenly hold at a stale opacity or jump — `tick` should keep
        // moving it every call, toward whatever the current target is.
        let mut fade = WispFade::arriving();
        fade.tick(ARRIVE_DURATION_SECS * 0.5); // partway arrived
        let partial = fade.opacity();
        assert!(partial > 0.0 && partial < 1.0);

        fade.begin_depart();
        fade.tick(0.01);
        assert!(fade.opacity() < partial, "must start moving toward zero immediately");
    }

    // ── classify_roster ──

    fn set(items: &[&str]) -> HashSet<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn classify_splits_appeared_departed_and_persisted() {
        let known = set(&["mcp/toad", "kaijutsu-app", "mcp/gone"]);
        let live = set(&["mcp/toad", "kaijutsu-app", "mcp/fresh"]);
        let delta = classify_roster(&known, &live);

        let mut appeared = delta.appeared.clone();
        appeared.sort();
        assert_eq!(appeared, vec!["mcp/fresh".to_string()]);

        let mut departed = delta.departed.clone();
        departed.sort();
        assert_eq!(departed, vec!["mcp/gone".to_string()]);

        let mut persisted = delta.persisted.clone();
        persisted.sort();
        assert_eq!(persisted, vec!["kaijutsu-app".to_string(), "mcp/toad".to_string()]);
    }

    #[test]
    fn classify_handles_empty_known_as_all_appeared() {
        let known = HashSet::new();
        let live = set(&["mcp/toad", "kaijutsu-app"]);
        let delta = classify_roster(&known, &live);
        assert!(delta.departed.is_empty());
        assert!(delta.persisted.is_empty());
        let mut appeared = delta.appeared;
        appeared.sort();
        assert_eq!(appeared, vec!["kaijutsu-app".to_string(), "mcp/toad".to_string()]);
    }

    #[test]
    fn classify_handles_empty_live_as_all_departed() {
        let known = set(&["mcp/toad", "kaijutsu-app"]);
        let live = HashSet::new();
        let delta = classify_roster(&known, &live);
        assert!(delta.appeared.is_empty());
        assert!(delta.persisted.is_empty());
        let mut departed = delta.departed;
        departed.sort();
        assert_eq!(departed, vec!["kaijutsu-app".to_string(), "mcp/toad".to_string()]);
    }

    // ── reconcile_seats (Bevy glue) ──

    /// The defect a first draft of `reconcile_seats` had: an early-return
    /// short-circuit fired whenever `appeared`/`departed` were both empty —
    /// which is the COMMON steady-state frame, since every unchanged nick
    /// falls into `persisted` every single poll. That skipped the
    /// `persisted` loop entirely, so a nick that reappeared mid-fade-out
    /// (present in both `known` and `live` — classified as `persisted`, not
    /// `appeared`) never got its `begin_arrive()` recovery call when it was
    /// the ONLY thing that changed that frame. Guards against reintroducing
    /// it (the switchboard sibling module has the same style of regression
    /// test for its own analogous found bug).
    #[test]
    fn reconcile_recovers_a_persisting_nick_mid_depart_even_when_nothing_else_changed() {
        use bevy::ecs::system::RunSystemOnce;
        use kaijutsu_client::PeerInfo;

        let mut app = App::new();
        app.add_plugins(bevy::time::TimePlugin)
            .insert_resource(Assets::<Mesh>::default())
            .insert_resource(Assets::<StandardMaterial>::default())
            .init_resource::<SeatsState>();

        app.world_mut().spawn(super::super::RoomRoot);
        let wisp_entity = app.world_mut().spawn(Wisp { nick: "mcp/toad".to_string() }).id();

        // Seed a wisp already mid-fade-out — the state a departed-then-
        // reappeared peer would be in when this reconcile pass runs.
        let mut fade = WispFade::arriving();
        fade.opacity = 1.0; // was fully arrived
        fade.begin_depart();
        fade.tick(1.0); // now partway faded out
        assert!(fade.opacity() > 0.0 && fade.opacity() < 1.0, "sanity: fixture must start mid-fade");
        app.world_mut()
            .resource_mut::<SeatsState>()
            .wisps
            .insert("mcp/toad".to_string(), WispRuntime { entity: wisp_entity, fade });

        // The roster shows this nick still (again) live — no OTHER nick
        // appeared or departed this frame, so `persisted` is the only
        // non-empty bucket.
        let mut roster = PeerRoster::default();
        roster.peers = vec![PeerInfo { nick: "mcp/toad".to_string(), attached_at: 0 }];
        roster.loaded = true;
        app.insert_resource(roster);

        app.world_mut().run_system_once(reconcile_seats).unwrap();

        let state = app.world().resource::<SeatsState>();
        let runtime = state.wisps.get("mcp/toad").expect("nick stays tracked, not re-spawned");
        assert_eq!(runtime.entity, wisp_entity, "must not spawn a duplicate for an already-known nick");
        assert_eq!(runtime.fade.target, 1.0, "the recovery — the only change this frame — must still fire");
    }
}
