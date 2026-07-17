//! Time-well 3D scene: camera, root, screen toggle, billboarding, and card
//! motion. Owns everything that is *not* the keyed-join sync (which lives in
//! [`super::sync`]) and not the pure model (which lives in [`super::card`]).

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use bevy::prelude::*;
use kaijutsu_types::ContextId;
use kaijutsu_viz::layout::Band;

use super::card::CardData;
use crate::ui::screen::Screen;
use crate::view::scene_palette::ScenePalette;
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
    /// The selected card's live-tail band content (the last few tail lines,
    /// pre-shaped by [`super::live::tail_lines`]), `None` for every
    /// non-selected card. Written by [`super::live::sync_selected_card_tail`]
    /// **only when it actually changes** — the same guarded-write discipline
    /// as `selected`/`in_lineage`/`drifting` above, which rides the existing
    /// `Changed<Card>` gate `text::build_card_scenes` already has rather than
    /// adding a second rebuild path (`docs/timewell.md`'s HUD melt, replacing
    /// the South HUD panel's job on the card face itself).
    pub tail: Option<String>,
}

/// The placement/root entity that re-roots the well's whole subtree into room
/// space (Slice B, mirrors patch bay's `StationWPlacement` + `PatchBayRoot`
/// folded into one entity — the well needs no separate placement/content
/// split since, unlike the wall-mounted wheel, it has no change-of-basis
/// rotation to compose). Carries [`placement_transform`] (identity this
/// slice — see [`IDENTITY_PLACEMENT`]); every card/ring/ray/label is its
/// `ChildOf` descendant, so `RoomRoot`'s own recursive despawn (Slice D: the
/// well only ever enters as room furniture now, `spawn_well_furniture`
/// parents this under `RoomRoot`) despawning this ONE entity (recursively)
/// is the whole teardown — no per-type queries, no leaked entity. Does NOT
/// own the well camera: that's the app's one shared `Camera3d`, repurposed
/// via marker component (`crate::view::room::RoomCamera`), never a child of
/// anything well-specific.
#[derive(Component)]
pub struct TimeWellRoot;

/// The center-bottom reading slot: a single large card floating at a fixed
/// position at the well's mouth ([`FOCUS_CARD_POS`]), billboarded like every
/// other card — `ChildOf` the well's placement root (Slice B), not the
/// camera, despite this doc's old claim. Renders the current selection at
/// readable size; updated by `text::update_reading_card` on selection change.
#[derive(Component)]
pub struct ReadingCard;

/// The well's base ring deck: a flat disc behind the cards (XY plane) that
/// renders the concentric rings + spiral core + activity ripples (the well's
/// "pulse"). Driven by [`WellRingsMaterial`] uniforms from [`tick_and_sync_rings`].
/// Despawned on exit alongside the root.
#[derive(Component)]
pub struct WellRingsDeck;

/// A magic-circle ring for one band (the Konosuba/"Explosion"-spell aesthetic —
/// concentric glyph rings, counter-rotating, receding into the funnel). One
/// entity per band ring (see [`super::card::terrace_ring_geometry`]), driven by
/// [`crate::shaders::TerraceRingMaterial`]. Carries its **ring index** (= band
/// index, 0 = `Active` … `N_BANDS-1` = `Demoted`) so [`dim_nonfocused_rings`]
/// can brighten the focused ring and dim the rest. Despawned on exit alongside
/// the rest of the well.
#[derive(Component)]
pub struct TerraceRing(pub usize);

/// The event-horizon "+N" count label, parked at the funnel center beyond the
/// deepest ring ([`super::card::horizon_label_pos`]). Its text changes (the
/// horizon count), refreshed by [`super::text::build_horizon_label`] only when
/// the count actually changes. Despawned on exit alongside the rest of the
/// well. (The per-band "ACTIVE"/"RECENT" ring labels that once shared this
/// path were removed 2026-07-06 — the reading card's SPECS `band` line
/// carries that information without cluttering the rings.)
#[derive(Component)]
pub struct HorizonLabel;

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
/// map, the per-ring seat orders, and the shared mesh / per-accent material
/// handles built on first enter.
#[derive(Resource)]
pub struct TimeWellState {
    pub join: kaijutsu_viz::join::Join<ContextId, kaijutsu_client::ContextInfo>,
    pub entities: HashMap<ContextId, Entity>,
    /// Shared quad mesh for every card (built lazily on first enter).
    pub card_mesh: Option<Handle<Mesh>>,
    /// Each ring's seated cards in seat order, indexed by
    /// [`kaijutsu_viz::layout::Band::index`] — the ring-centric nav's source of
    /// truth. `(focused_ring, ring_pos)` indexes into `ring_cards[focused_ring]`
    /// to resolve [`selected`](Self::selected), and digit keys `0-9` address
    /// seats 0-9 of the focused ring directly. Rebuilt each layout tick from
    /// [`super::card::assign_placement`]'s `rings`.
    pub ring_cards: [Vec<ContextId>; super::card::N_BANDS],
    /// Count of contexts past seat 9 of `Recent`/`Bumped`/`Demoted` (plus
    /// already-filtered archived contexts) — the event horizon. No card
    /// entity ever exists for these; the throat renders this as a "+N" count.
    /// Rebuilt each layout tick alongside `ring_cards`.
    pub horizon_count: usize,
    /// The `visible` (non-archived) context count as of the last layout tick
    /// — `sync_time_well`'s change-detection dodge. Compared against
    /// `visible.len()`, not `join.len()`, because horizon contexts never
    /// enter the join, so the two can legitimately differ.
    pub last_seen_visible_count: usize,
    /// Which band ring is currently focused (0 = `Active` at the mouth …
    /// `N_BANDS-1` = `Demoted` at the throat). Up/Down change it.
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
    /// the `get_clusters` poll. Drives the cluster label on `Demoted` cards
    /// (Stage 3 will extend this to cluster-grouped angle within a band; for
    /// now it's label-only — see `card::card_from`). Empty when the kernel has
    /// no semantic index.
    pub cluster_of: HashMap<ContextId, super::card::ClusterAssignment>,
    /// Contexts with a placement verb (`p`/`d`/`z`/`a`) in flight — fired over
    /// RPC but not yet reflected by a `DriftState` poll. Further placement
    /// verbs on a pending context are ignored (info-logged) until the next
    /// poll clears the set ([`super::sync::sync_time_well`]): without this,
    /// a double-`d` inside one poll interval walks the ladder two rungs
    /// sight-unseen (auto → demoted → ARCHIVED). Digits/Enter/`c` stay
    /// unguarded — navigation and conclude aren't ladder steps.
    pub placement_pending: HashSet<ContextId>,
    /// Whether the camera is parked in the **hero pose** — Up at the mouth
    /// ring (already the shallowest; nowhere further inward to focus) rises
    /// into an elevated, looking-down establishing shot of the well *within
    /// the room* instead of a dead-end no-op (Amy: "a final up arrow took
    /// the camera to focus on the well from a bit above looking down...
    /// esp with the room to give it perspective"). A look-only dead end
    /// while active — [`well_keyboard`] ignores everything except Down
    /// (returns to the mouth ring's normal framing) and Esc (leaves the well,
    /// same as from any ring-overview). Drives
    /// [`crate::view::room::shot::RoomShot::WellHero`] in `ease_shell_camera`.
    pub hero: bool,
    /// Force [`super::text::build_card_scenes`] to rebuild EVERY rim card's
    /// MSDF text on its next run, not just the ones `Changed<Card>` would
    /// catch (Slice C: that system is now dived-only — room-scale text is
    /// unreadably small pixels, so it doesn't run at all while ambient). Set
    /// by [`arm_dive`] on every zoom-in; cleared by `build_card_scenes` once
    /// it actually rebuilds. Mirrors `patch_bay::PatchBayState::arm_text`'s
    /// dirty-flag shape.
    pub card_text_dirty: bool,
    /// The one shared material every [`super::drape::LineageDrape`] ribbon
    /// reuses — built lazily on first use by
    /// [`super::drape::sync_lineage_drapes`], same "build once, cache the
    /// handle" shape as [`Self::card_mesh`].
    pub lineage_drape_material: Option<Handle<crate::shaders::TraceGlowMaterial>>,
}

impl TimeWellState {
    /// Try to start a placement verb on `id`: `true` marks it in flight;
    /// `false` means one is already pending (caller skips + logs). Cleared
    /// wholesale when the next `DriftState` poll lands.
    pub fn begin_placement(&mut self, id: ContextId) -> bool {
        self.placement_pending.insert(id)
    }
}

impl Default for TimeWellState {
    fn default() -> Self {
        Self {
            join: kaijutsu_viz::join::Join::new(),
            entities: HashMap::new(),
            card_mesh: None,
            ring_cards: std::array::from_fn(|_| Vec::new()),
            horizon_count: 0,
            last_seen_visible_count: 0,
            focused_ring: 0,
            ring_pos: 0,
            ring_rotation: [0.0; super::card::N_BANDS],
            ring_rotation_target: [0.0; super::card::N_BANDS],
            selected: None,
            focused: false,
            cluster_of: HashMap::new(),
            placement_pending: HashSet::new(),
            hero: false,
            // The very first dive also needs its card text built — there is
            // no prior `arm_dive` call to have armed it (mirrors
            // `RoomState::plates_dirty`'s "fresh state starts dirty" stance).
            card_text_dirty: true,
            lineage_drape_material: None,
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

/// In-world label quad size (well units) — modest, well under a rim card's
/// [`CARD_WIDTH`]/[`CARD_HEIGHT`], so the four ring labels and the "+N" horizon
/// count read as passive structural annotations, not competing content.
const LABEL_QUAD_W: f32 = 120.0;
const LABEL_QUAD_H: f32 = 38.0;

/// Label texture size (logical px). Small and wide — short strings only
/// ("ACTIVE"/"RECENT"/"BUMPED"/"DEMOTED"/"+N"). `pub` (like [`CARD_TEX_W`]/
/// [`READING_TEX_W`]) so `text.rs`'s label-layout systems can size to it.
pub const LABEL_TEX_W: f32 = 220.0;
pub const LABEL_TEX_H: f32 = 70.0;

/// Fixed brightness multiplier ([`crate::shaders::WellCardMaterial::dim`].x)
/// for labels — LDR (< 1.0), never touched per-frame. The HDR-tiering rule
/// (`docs/timewell.md` appendix) reserves bloom (> 1.0) for live action
/// (selection rims, status pulses); a ring label is passive structural state,
/// so it stays dim and constant rather than reactive to focus.
const LABEL_DIM: f32 = 0.85;

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
/// sub-rings + two-tier spokes + dashed inner arc need room to read).
/// **Amy-tunable.**
const TERRACE_RING_BAND_HALF_WIDTH: f32 = 0.09;

/// Base spin rate (radians/sec-ish, tune by eye) for terrace ring `k`; each
/// deeper ring spins a touch faster so the funnel reads as receding motion.
/// **Amy-tunable — kept calm for the first cut.** `pub(crate)`: this is
/// also the master gear of the room's kinetic ensemble — the FSN portal's
/// orbit derives from it (`fsn::layout::ORBIT_RATE` = this ×
/// `ORBIT_GEAR_RATIO`; Amy 2026-07-13: the two motions read gear-like, so
/// they should BE geared — retune this one number and the whole train
/// turns together).
pub(crate) const TERRACE_RING_SPIN_BASE: f32 = 0.13;
const TERRACE_RING_SPIN_STEP: f32 = 0.04;

/// Overall alpha/intensity for the terrace rings. Started at 0.35 ("kept low
/// for the first cut so the driver tunes up") — tuned up to 0.55 2026-07-12
/// after Amy read the new centerpiece mix as "muted like the rest of the
/// octagon": the variant glyphs + gem glints earn more presence, and the
/// well's HDR-forward look is the reference. **Amy-tunable**; the deeper fix
/// (unified brightness/HDR tiers) is the color-management pass in issues.md.
const TERRACE_RING_ALPHA: f32 = 0.55;

// The well's neon hue — an indigo/blue-violet bridging the old saturated
// cyan-blue (`WellRingsMaterial`'s `ring_color`, formerly `[0.35, 0.62,
// 1.0]`, and the terrace ring's own formerly icy cyan-white) toward the
// room's information/radiator violet — moved onto `ScenePalette::neon`
// (shared by the ring deck's `ring_color` and the terrace glyph rings so the
// well's two neon layers read as one family, the way the gold core and the
// room's own gold trim already do). The terrace glyph color (a lighter,
// softer tint of the deck's neon) moved onto `ScenePalette::terrace`.

/// Brightness multiplier for rings + cards **not** on the focused ring, so the
/// focused ring clearly pops (a card parked in front by a neighbor ring no
/// longer out-shines the ring you're navigating). 1.0 = full. **Amy-tunable.**
const DIM_NONFOCUSED: f32 = 0.25;
/// Easing rate for the focus dim, so Up/Down fades between rings rather than
/// snapping. **Amy-tunable.**
const DIM_EASE_RATE: f32 = 6.0;

/// Per-band recline (radians) layered on the billboard. With the whole funnel now
/// reclined in space (see [`super::card::WELL_TILT`]), the cards read their 3D
/// form from their *positions* and should face the camera cleanly, so the recline
/// is off. Kept as a per-band knob in case a slight lean reads better. Tunable.
fn card_tilt(band: Band) -> f32 {
    match band {
        Band::Active => 0.0,
        Band::Recent => 0.0,
        Band::Bumped => 0.0,
        Band::Demoted => 0.0,
    }
}

/// Exponential-smoothing rate for card motion (higher = snappier).
const CARD_EASE_RATE: f32 = 8.0;

/// Snap-and-hold threshold for the per-frame easing systems: once an eased value
/// is within this of its target it's snapped to the exact target **once** and
/// then left alone, so a settled entity stops writing (no per-frame
/// `Assets::get_mut` re-extract, no `Changed<Transform>` at rest). Unitless
/// values (dim, scale, rotation) use this directly; card *position* uses the
/// squared world-distance [`CARD_SETTLE_DIST_SQ`].
const SETTLE_EPS: f32 = 1e-4;

/// Squared world-distance under which a card is "arrived" at its target and snaps
/// (world units² — cards sit at radius ~300–500, so 0.01 ≈ 0.1u is imperceptible).
const CARD_SETTLE_DIST_SQ: f32 = 0.01;

/// Exponential-smoothing rate for the per-ring projector spin (how fast a ring
/// rotates its selected card to the gate). **Amy-tunable.**
const RING_SPIN_EASE_RATE: f32 = 6.0;

/// Blend dial for rim-card orientation: `0.0` = today's full camera-billboard,
/// `1.0` = full ring-aligned (cards stand around their ring like a carousel,
/// face-normal radial-outward, up along the funnel axis). Intermediate values
/// slerp between the two. The focus [`ReadingCard`] is unaffected (always
/// billboarded). **Amy-tunable.**
const RING_ALIGN: f32 = 1.0;

// The well's own `CAMERA_EASE_RATE` (and `ease_camera_to_focused_ring`, the
// system it drove) is gone (Slice C): the well no longer has its own camera
// — `room::ease_shell_camera` eases the ONE shared camera toward whatever
// `shot::resolve` returns, at `room::CAMERA_EASE_RATE`, for every screen the
// shell can be in, well included.

// ============================================================================
// PLACEMENT (Slice B seam, `lovely-swimming-prism.md` — mirrors patch bay's
// `StationPlacement`, `patch_bay/mod.rs:71-98,275-293`)
// ============================================================================

/// A rigid-plus-uniform-scale placement of the well into room space — the same
/// shape as patch bay's `StationPlacement`, minus the pitch/yaw pair: the well
/// needs no wall-mount change-of-basis (its recline already lives in the
/// geometry itself, [`super::card::well_tilt_quat`]), so a single `Quat`
/// covers whatever rotation a later slice needs.
pub struct StationCenterPlacement {
    /// Room-space translation of the well's local origin.
    pub translation: Vec3,
    /// Uniform scale applied to the well's local coordinates.
    pub scale: f32,
    /// Placement rotation. Identity for this slice; a later slice may need a
    /// real one to re-seat the well non-trivially at the room's center.
    pub rotation: Quat,
}

/// Slice B's placement: identity — no visual change. Every well spawn site
/// re-roots under this so the mechanical reparenting (this slice's whole job)
/// is proven safe before a LATER slice replaces these *values* (never this
/// shape) to seat the well at the room's center. Superseded as the
/// PRODUCTION placement by [`STATION_CENTER_PLACEMENT`] (Slice C) — kept
/// alive for its own `placement_*` no-op tests below, the same role
/// `IDENTITY_PLACEMENT`-shaped constants play as a baseline proof elsewhere.
/// `#[allow(dead_code)]`: its only reachable caller now is `#[cfg(test)]`
/// code, which a plain `cargo build` doesn't compile — not unused in
/// `cargo test`.
#[allow(dead_code)]
pub const IDENTITY_PLACEMENT: StationCenterPlacement = StationCenterPlacement {
    translation: Vec3::ZERO,
    scale: 1.0,
    rotation: Quat::IDENTITY,
};

/// Uniform scale from the well's native ~500-unit mouth radius
/// (`super::card::SPIRAL_R_MOUTH`, private to `card` — see its
/// `band_ring`) down to roughly match the room's existing
/// `room::TABLE_PLINTH_RADIUS` (145): 500 × 0.3 = 150, bumped to 0.5 (250)
/// after Amy's live look — the console read as too small a presence next to
/// the room's other furniture at the first guess. **Still a live-tuning
/// value** (lovely-swimming-prism.md, Slice C), same spirit as
/// `patch_bay::STATION_W_SCALE`'s own 0.34 → 0.66 retune.
const STATION_CENTER_SCALE: f32 = 0.5;

/// Extra lift stacked on top of [`crate::view::room::TABLE_TOP_Y`] (the
/// room table's top face) for [`STATION_CENTER_PLACEMENT`]'s translation.
/// Needed because the mouth ring's *center of rotation* sits at local y=0
/// (band 0's `depth` is 0 in [`super::card::band_ring`]), but its actual
/// seated cards do NOT — the funnel recline ([`super::card::well_tilt_quat`],
/// `WELL_TILT ≈ -0.95` rad) tips the ring so its seats swing roughly
/// `±(mouth_radius × cos(WELL_TILT))` ≈ ±290 world-units in **local y** around
/// that center (verified by hand against `ring_seat_rotated`'s rotation
/// matrix), not just sitting flat at y=0. Scaled by
/// [`STATION_CENTER_SCALE`] that dip is ≈ ∓87 units — left unlifted, the
/// bottom of the mouth ring would clip a little way into the tabletop.
/// **First guess — live-tune over BRP**, same as the scale above; this
/// isn't meant to be derived to the millimeter here, just kept in the right
/// ballpark with the reasoning written down for whoever tunes it next.
const STATION_CENTER_LIFT: f32 = 90.0;

/// The well's production placement (Slice C, `lovely-swimming-prism.md`):
/// seats the ring-carousel at the room's center, hovering above the existing
/// `room::spawn_table`'s top face — the well's rings ride the room's own
/// table furniture rather than replacing it (see `room::mod.rs`'s
/// `spawn_table` doc + this plan's step 9 on `RoomDistraction`). No rotation
/// — the well's own recline already lives in its geometry
/// ([`super::card::well_tilt_quat`]), so the room-space seam only needs to
/// translate + scale it, same reasoning [`StationCenterPlacement`]'s own doc
/// gives for skipping a pitch/yaw pair. Both constants above are explicitly
/// first guesses for the lead to live-tune over BRP, not finished numbers.
/// `LazyLock`, not `const`: [`Quat::from_rotation_x`] below isn't a `const
/// fn` (it calls `sin`/`cos`, not yet const on stable) — a plain `const`
/// declaration can't call it. `Deref` makes every existing `&STATION_CENTER_PLACEMENT`/
/// `STATION_CENTER_PLACEMENT.field` call site work unchanged.
pub static STATION_CENTER_PLACEMENT: LazyLock<StationCenterPlacement> = LazyLock::new(|| StationCenterPlacement {
    translation: Vec3::new(0.0, crate::view::room::TABLE_TOP_Y + STATION_CENTER_LIFT, 0.0),
    scale: STATION_CENTER_SCALE,
    // Amy (live, first look at Slice C): "needs to be realigned with the
    // room... I think the magic rings can be parallel to the floor." With
    // `Quat::IDENTITY` here the deck/terrace-ring quads (local normal +Z,
    // tipped back by `super::card::WELL_TILT` ≈ -0.95 rad about X before
    // this placement ever touches them) sit at a ~54° lean off the floor —
    // the room's own horizontal read (floor/table are level; only the
    // wheel's WALL mount and this console lean at all). Countering with an
    // X rotation of `-FRAC_PI_2 - WELL_TILT` composes with that baked-in
    // tilt (same-axis rotations add: net angle = this + WELL_TILT) to land
    // the net rotation at exactly `-FRAC_PI_2` — local +Z all the way to
    // world +Y, the deck flat and face-up toward the overview camera sitting
    // above it. First live-tuning pass, not a final answer: this ALSO
    // re-reads each band's depth step (`card::RING_DEPTH_STEP`, along local
    // Z) as a vertical stack rather than a receding tilt-plane depth — watch
    // for whether that reads as "concentric flat rings" or "a tiered stack"
    // once it's on screen, and back off the angle (partial recline instead
    // of full flatten) if the stacking reads wrong.
    rotation: Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2 - super::card::WELL_TILT),
});

/// The `Transform` for the placement/root entity that re-roots the well's
/// whole subtree into room space — mirrors patch bay's `placement_transform`
/// (`patch_bay/mod.rs:275-279`). [`spawn_well_furniture`] spawns
/// [`TimeWellRoot`] with this; every card/ring/ray/label hangs off it via
/// `ChildOf`.
pub fn placement_transform(p: &StationCenterPlacement) -> Transform {
    Transform::from_translation(p.translation)
        .with_rotation(p.rotation)
        .with_scale(Vec3::splat(p.scale))
}

/// Map a well-LOCAL point to room space through a placement — the same
/// similarity transform [`placement_transform`] applies to the subtree, as a
/// point mapping a caller can use without spawning an entity. Unlike patch
/// bay's `#[cfg(test)]`-only twin (`patch_bay/mod.rs:290-293`), this one is
/// NOT test-only: a later slice's camera-shot resolver composes the well's
/// local camera poses (today's `ease_camera_to_focused_ring` math) through
/// this placement, so it has to be a real, callable function from the start.
pub(crate) fn placement_to_room(p: &StationCenterPlacement, local: Vec3) -> Vec3 {
    p.translation + p.rotation * (local * p.scale)
}

// ============================================================================
// ENTER / EXIT
// ============================================================================

/// Spawn the well's furniture — the placement/root
/// ([`STATION_CENTER_PLACEMENT`], Slice C's room-center seat) + ring deck +
/// terrace rings + focus card + horizon label, every entity `ChildOf` the
/// placement. `parent`, when given, re-parents the placement root under it —
/// `room::enter_room` passes `RoomRoot` (the well is room furniture, spawned
/// once per room visit alongside the patch bay). `None` has no live caller
/// left now that `Screen::TimeWell`'s direct-entry path is gone (Slice D);
/// the parameter stays `Option` rather than dropping to a bare `Entity`
/// since that's a signature change beyond this cleanup's scope.
///
/// Not a Bevy system — a plain function taking `&mut Commands`/`&mut Assets`,
/// the same shape `patch_bay::spawn_furniture` uses, so `room::enter_room`
/// can call it directly alongside its own furniture spawns.
pub(crate) fn spawn_well_furniture(
    commands: &mut Commands,
    parent: Option<Entity>,
    state: &mut TimeWellState,
    palette: &ScenePalette,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<crate::shaders::WellCardMaterial>,
    ring_materials: &mut Assets<crate::shaders::WellRingsMaterial>,
    terrace_ring_materials: &mut Assets<crate::shaders::TerraceRingMaterial>,
    images: &mut Assets<Image>,
) -> Entity {
    // Fresh entry always starts in the overview (not focused).
    state.focused = false;

    // Build the shared rim-card block once (a thin 3D box, not a flat quad, so
    // both large faces read from their own side — see [`card_block_mesh`]).
    if state.card_mesh.is_none() {
        state.card_mesh = Some(meshes.add(card_block_mesh()));
    }

    // The placement/root entity (Slice C: seated at the room's center via
    // `STATION_CENTER_PLACEMENT`, no longer the identity placement Slice B
    // proved the reparenting with). `Name` says "Placement" (not "Root") for
    // debuggability, matching patch bay's naming; the `TimeWellRoot` marker is
    // what the per-frame systems below (and teardown) query for.
    let mut root_entity = commands.spawn((
        TimeWellRoot,
        placement_transform(&STATION_CENTER_PLACEMENT),
        Visibility::Inherited,
        Name::new("TimeWellPlacement"),
    ));
    if let Some(p) = parent {
        root_entity.insert(ChildOf(p));
    }
    let root = root_entity.id();

    // Base ring deck: a flat disc behind the cards that renders the well's pulse
    // (concentric rings + spiral core + activity ripples). Driven per-frame by
    // `tick_and_sync_rings`. Not billboarded — it faces the camera (+Z) as a
    // fixed floor; the shader fades its square corners to nothing.
    let deck_mesh = meshes.add(Rectangle::new(RING_DECK_SIZE, RING_DECK_SIZE));
    // Warm gold core (`ScenePalette::gold` — was `[1.0, 0.62, 0.20]`, the
    // concept-art palette's own warm gold; leaned the rest of the way onto the
    // room's shared gold) + indigo-violet neon rings (`ScenePalette::neon` —
    // was `[0.35, 0.62, 1.0]`, a saturated cyan-blue). Room-palette re-skin,
    // 2026-07-11: HDR-on-activity behavior (`energy`/ripples in
    // `well_rings.wgsl`) is untouched — only these RESTING identity hues moved.
    let deck_material = ring_materials.add(crate::shaders::WellRingsMaterial::new(
        Vec4::new(palette.gold.red, palette.gold.green, palette.gold.blue, 1.0),
        ScenePalette::vec3(palette.neon).extend(1.0),
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
        ChildOf(root),
    ));

    // Band magic-circle rings: one annulus quad per band ring (the
    // Konosuba/"Explosion"-spell aesthetic), counter-rotating and receding into
    // the funnel on the same tilted axis as the deck/cards. Each quad is sized
    // to ITS ring's radius, and the band is drawn centered on that radius so it
    // lands on the cards seated around the ring.
    let ring_geometry = super::card::terrace_ring_geometry();
    let ring_count = ring_geometry.len();
    for (k, (radius, depth)) in ring_geometry.into_iter().enumerate() {
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
            ScenePalette::vec3(palette.terrace),
            TERRACE_RING_ALPHA,
            k,
            ring_count,
            ScenePalette::vec3(palette.gold),
        ));
        let ring_pos = tilt * Vec3::new(0.0, 0.0, depth);
        commands.spawn((
            TerraceRing(k),
            Mesh3d(ring_mesh),
            MeshMaterial3d(ring_material),
            Transform {
                translation: ring_pos,
                rotation: tilt,
                scale: Vec3::ONE,
            },
            Visibility::Inherited,
            Name::new(format!("TerraceRing{k}")),
            ChildOf(root),
        ));
    }

    // Focus card: an in-world 3D card floating lower-center at the mouth of the
    // well (not a flat HUD panel — it lives in the scene, billboarded, and the
    // camera dollies into it on focus). It renders the current selection;
    // `update_reading_card` fills its texture (blank until a selection exists).
    let focus_mesh = meshes.add(Rectangle::new(FOCUS_QUAD_W, FOCUS_QUAD_H));
    let (focus_image, panel) =
        create_msdf_panel(images, READING_TEX_W as u32, READING_TEX_H as u32);
    let focus_material = materials.add(crate::shaders::WellCardMaterial {
        texture: focus_image,
        accent: Vec4::ZERO, // filled by update_reading_card on the first selection
        params: Vec4::ZERO,
        shape: card_shape(),
        border: Vec4::ZERO,
        // dim.x = 1: never dimmed (not a rim Card). y/z are the live
        // chatter/beat lanes — MUST stay 0 (Vec4::ONE here lit both full-on:
        // the accidental cyan+gold "cream ring" fixed 2026-07-06).
        dim: Vec4::new(1.0, 0.0, 0.0, 0.0),
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
        ChildOf(root),
    ));

    // Event-horizon "+N" count label: parked at the funnel center beyond the
    // deepest ring. Refreshed by `text::build_horizon_label` whenever the
    // count changes (it starts blank — nothing polled yet on first enter).
    // Spawns `Visibility::Inherited`, but the ambient `apply_horizon_label_lod`
    // corrects it to Hidden at room scale every frame (freeze-fix slice,
    // 2026-07-11) — left ungated it stood as an unreadable dark chip
    // floating over the well outside the dive (live-observed).
    let label_mesh = meshes.add(Rectangle::new(LABEL_QUAD_W, LABEL_QUAD_H));
    let (horizon_image, horizon_panel) =
        create_msdf_panel(images, LABEL_TEX_W as u32, LABEL_TEX_H as u32);
    let horizon_material = materials.add(crate::shaders::WellCardMaterial {
        texture: horizon_image,
        accent: Vec4::ZERO,
        params: Vec4::ZERO,
        shape: label_shape(),
        border: Vec4::ZERO,
        // dim.x only — y/z are live chatter/beat lanes, not brightness.
        dim: Vec4::new(LABEL_DIM, 0.0, 0.0, 0.0),
    });
    commands.spawn((
        HorizonLabel,
        Mesh3d(label_mesh),
        MeshMaterial3d(horizon_material),
        Transform::from_translation(super::card::horizon_label_pos()),
        Visibility::Inherited,
        horizon_panel,
        Name::new("HorizonLabel"),
        ChildOf(root),
    ));

    info!("time-well: furniture spawned (room-center placement)");
    root
}

/// Re-arm [`TimeWellState`]/[`super::rays::WellTracks`] for a fresh room
/// entry, called from `room::enter_room` right after [`spawn_well_furniture`]
/// (mirrors `patch_bay::arm_scene`). `spawn_well_furniture` just built
/// brand-new (empty) furniture, but `state.entities`/`state.join` and
/// `tracks`' own id→entity map are Resources that survive across room
/// visits — without clearing them here, `sync_time_well`/`sync_track_rays`
/// would think the entities from the PREVIOUS visit (long since despawned
/// with the old `RoomRoot`) are still current and never rebuild a single
/// card or ray. Deliberately narrow: ring-centric nav state (`ring_cards`,
/// `focused_ring`, `ring_pos`, `ring_rotation*`, `selected`,
/// `placement_pending`) is untouched — it isn't tied to any specific entity,
/// so it persists naturally across a room round-trip (the "resume where you
/// left off" stance this plan also takes for [`arm_dive`]).
pub fn arm_well(state: &mut TimeWellState, tracks: &mut super::rays::WellTracks) {
    state.entities.clear();
    state.join = kaijutsu_viz::join::Join::new();
    state.last_seen_visible_count = 0;
    // Correction from a design review of this plan: without ALSO clearing
    // this, `sync_track_rays`'s re-entry count fallback
    // (`ray_entities.len() == tracks.len()`) can match by COUNT ALONE against
    // a stale roster of dead entity ids left over from the previous visit,
    // and silently never respawn a single ray.
    tracks.clear_ray_entities();
}

/// Re-arm [`TimeWellState`] for a fresh dive (zoom-in), called from
/// `room_keyboard`'s Enter-on-`TimeWell` branch (mirrors
/// `patch_bay::PatchBayState::arm_text`, one field at a time rather than a
/// single dirty bit, since the well has more per-dive state to reset).
/// Resets `focused` (a fresh dive always starts at the ring overview, not
/// mid-focus-on-a-card), `placement_pending` (a stale in-flight guard from
/// a much earlier visit shouldn't block this dive's first placement verb),
/// and `hero` (found by a kaibo review round, 2026-07-11: unlike ring
/// position/rotation, the hero pose has no visual anchor a user would
/// recognize as "where I left off" — leaving it set stuck every re-dive
/// after a hero visit in the elevated establishing shot, ignoring all input
/// but Down/Esc, until they pressed Down once to escape it). Deliberately
/// does **not** reset `ring_rotation`/`ring_rotation_target` — ring focus/
/// position persists across zoom out/in ("resume where you left off");
/// resetting only one of that pair would cause a spurious spin on every
/// dive, worse than resetting neither (a design-review correction to this
/// plan's first pass). Also arms [`TimeWellState::card_text_dirty`] so
/// `text::build_card_scenes` — dived-only, since room-scale card text is
/// unreadable pixels — rebuilds every rim card's glyphs on the way in, not
/// just the ones `Changed<Card>` would happen to catch.
pub fn arm_dive(state: &mut TimeWellState) {
    state.focused = false;
    state.placement_pending.clear();
    state.card_text_dirty = true;
    state.hero = false;
}

/// Whether the room is currently zoomed onto the time well — the well's
/// dived-only `run_if` gate. A plain, directly-testable predicate (mirrors
/// `patch_bay::is_zoomed_into`'s pure half) rather than a `Res`-taking
/// system, since every call site here already holds a plain `&RoomState`.
pub fn well_zoomed(room: &crate::view::room::RoomState) -> bool {
    room.zoomed == Some(crate::view::room::nav::Station::TimeWell)
}

// ============================================================================
// TOGGLE
// ============================================================================

/// Ctrl+W as a **symmetric room toggle** (Slice C, `lovely-swimming-prism.md`
/// — a fable-review-proposed redesign of the plan's own first pass, which
/// would have accepted a Screen::Room "one more Esc hop" before reaching
/// Conversation). Reads only the CURRENT screen, never how it was reached —
/// no exit-path special-casing, the same anti-pattern Design C's
/// `Screen::TimeWell`-retirement reasoning rejects elsewhere in this plan:
/// - From `Screen::Conversation` → `Screen::Room`, focused on `TimeWell` and
///   already zoomed onto it (arming the dive) — the well replaces its old
///   direct scene-cut with "dive straight into the room's center furniture."
/// - From `Screen::Room` (zoomed on anything, or not) → straight back to
///   `Screen::Conversation` — the other half of the one-keystroke round trip.
/// - Any other screen: untouched (there's no evidence for a new case here).
///
/// Esc keeps its own strict well → room → conversation grammar everywhere
/// else ([`well_keyboard`]'s Escape branch, `room_keyboard`'s) — this binding
/// only ever short-circuits that with a direct hop, both ways.
/// Go to the well from anywhere — `Ctrl+A w`, `Ctrl+A "`, or gamepad Start
/// (docs/input.md; replaced the raw Ctrl+W toggle 2026-07-16 — Ctrl+W now
/// reaches kernel vi untouched in the editor, where it's the window verb).
///
/// From any screen, land dived into the well: carousel focused on it,
/// zoomed, dive armed. Already there → no-op (screen's `C-a w` re-lists;
/// leaving is Esc's job). From the editor this is a suspend-style exit —
/// the kernel session stays alive, same as its Ctrl+Z intercept.
pub fn handle_go_to_well(
    mut actions: MessageReader<crate::input::ActionFired>,
    screen: Res<State<Screen>>,
    mut next: ResMut<NextState<Screen>>,
    mut room: ResMut<crate::view::room::RoomState>,
    mut state: ResMut<TimeWellState>,
) {
    for crate::input::ActionFired { action, .. } in actions.read() {
        if !matches!(action, crate::input::Action::GoToWell) {
            continue;
        }

        let already_there = *screen.get() == Screen::Room
            && room.zoomed == Some(crate::view::room::nav::Station::TimeWell);
        if already_there {
            continue;
        }

        room.carousel = crate::view::room::nav::StationCarousel::new(
            crate::view::room::nav::Station::TimeWell,
        );
        room.zoomed = Some(crate::view::room::nav::Station::TimeWell);
        arm_dive(&mut state);
        if *screen.get() != Screen::Room {
            next.set(Screen::Room);
        }
    }
}

/// Time-well keyboard navigation: **ring-centric**, with a Kodak-projector spin.
/// Selection is `(focused_ring, ring_pos)`; the card at that seat is
/// [`TimeWellState::selected`], so the reading card / lineage / highlight /
/// Enter all follow.
///
/// Consumes `ActionFired` from the central table (the `WellZoomed` context in
/// `input/defaults.rs` — keys are rebindable there, this system only knows
/// intent):
/// - `JumpSeat(n)` (`0–9`) — quick-jump to seat `n` of the **focused** ring:
///   select + exit.
/// - `StepNext`/`StepPrev` (**Left / Right / Tab**, dpad) — step the position
///   within the focused ring (wrapping), spinning the ring so the selected
///   card eases to the front gate.
/// - `LevelUp`/`LevelDown` (**Up / Down**) — change the focused ring (Up →
///   shallower/mouth, Down → deeper/throat), carrying the position index
///   (clamped); the newly focused ring spins its selected card to the gate
///   and the camera retargets to it.
/// - `Activate` (**Enter**, South) focuses then commits; `Conclude` (`c`),
///   `Promote` (`p`), `Demote` (`d`), `PauseToggle` (`z`), `Archive` (`a`) —
///   see the verb arms below (fire-and-forget RPC).
///
/// PopLevel/Esc (focus-aware) is handled below: from focus it backs out to the ring
/// overview; from the overview it leaves the well — `room.zoomed = None`,
/// the same generic zoom-out every `station_is_zoomable` station uses now
/// (Slice C — the well is no longer a screen cut sitting BELOW the room, so
/// "leave the well" surfaces to the room's own ambient view, not straight to
/// Conversation the way the old `Screen::TimeWell` scene cut did).
pub fn well_keyboard(
    mut actions: MessageReader<crate::input::ActionFired>,
    mut state: ResMut<TimeWellState>,
    mut switch: MessageWriter<crate::view::components::ContextSwitchRequested>,
    mut next: ResMut<NextState<Screen>>,
    actor: Option<Res<crate::connection::RpcActor>>,
    drift: Res<crate::ui::drift::DriftState>,
    mut room: ResMut<crate::view::room::RoomState>,
) {
    use crate::input::Action;

    for crate::input::ActionFired { action, context } in actions.read() {
        // Only WellZoomed-context actions belong to the well; the context
        // stamp is what prevents buffered cross-frame actions (or another
        // station's same-key actions) from replaying here.
        if *context != crate::input::InputContext::WellZoomed {
            continue;
        }
        // The hero pose is a look-only dead end (see `TimeWellState::hero`'s
        // own doc): Down returns to the mouth ring's normal gate framing, Esc
        // leaves the well entirely (the same generic zoom-out as below) —
        // everything else is inert while parked here, since this pose frames
        // the well as a whole, not any one ring or card.
        if state.hero {
            match action {
                Action::LevelDown => state.hero = false,
                Action::PopLevel => room.zoomed = None,
                _ => {}
            }
            continue;
        }

        match action {
            // `0–9`: jump straight to seat `n` of the focused ring and drop
            // into the conversation (a no-op if that seat is empty).
            Action::JumpSeat(n) => {
                if let Some(&id) = state.ring_cards[state.focused_ring].get(*n) {
                    switch
                        .write(crate::view::components::ContextSwitchRequested { context_id: id });
                    next.set(Screen::Conversation);
                    return;
                }
            }

            // Left/Right (Tab = Right): walk the focused ring, wrapping, and
            // spin it so the newly selected seat rolls to the gate.
            Action::StepNext | Action::StepPrev => {
                let dir = if matches!(action, Action::StepNext) { 1 } else { -1 };
                let fr = state.focused_ring;
                let len = state.ring_cards.get(fr).map(|v| v.len()).unwrap_or(0);
                if len > 0 {
                    let pos = super::card::step_ring_pos(state.ring_pos, len, dir);
                    state.ring_pos = pos;
                    let cur = state.ring_rotation_target[fr];
                    state.ring_rotation_target[fr] =
                        super::card::spin_target_to_gate(cur, pos, len);
                    // Re-derive the selection from the seat so every
                    // downstream system (highlight, reading card, lineage)
                    // follows.
                    state.selected = state.ring_cards.get(fr).and_then(|v| v.get(pos)).copied();
                }
            }

            // Up/Down: change the focused ring (clamp 0..N_BANDS-1), carry the
            // position onto the new ring, spin the new ring to the gate. The
            // camera follows because `ease_shell_camera` keys on
            // `focused_ring` via `shot::WellShotInput`. Up at the mouth ring
            // (already the shallowest — nothing to clamp INTO) rises into the
            // hero pose instead of a dead-end no-op (`TimeWellState::hero`'s
            // own doc). The old speedbumped double-tap-to-Room edge
            // (`WellEdgeBump`) stays retired (Slice C): "leave the well" from
            // anywhere in this ladder is still just `room.zoomed = None` (see
            // PopLevel below), never a second screen.
            Action::LevelUp | Action::LevelDown => {
                let ud = if matches!(action, Action::LevelUp) { -1 } else { 1 };
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
                    state.selected = state
                        .ring_cards
                        .get(new_ring)
                        .and_then(|v| v.get(pos))
                        .copied();
                } else if ud == -1 && state.focused_ring == 0 {
                    state.hero = true;
                }
            }

            // Enter is two-stage: from the overview it *focuses* (the camera
            // dollies into the focus card); a second Enter while focused
            // *commits* — switches to the context and leaves the well.
            Action::Activate => {
                if state.focused {
                    if let Some(id) = state.selected {
                        switch.write(crate::view::components::ContextSwitchRequested {
                            context_id: id,
                        });
                        next.set(Screen::Conversation);
                    }
                } else if state.selected.is_some() {
                    state.focused = true;
                }
            }

            // Esc backs out: from focus it returns to the ring overview; from
            // the overview it leaves the well — `room.zoomed = None`, the same
            // generic zoom-out every zoomable station uses (not a `Screen`
            // transition; see this function's own doc for why the well no
            // longer jumps straight to Conversation here).
            Action::PopLevel => {
                if state.focused {
                    state.focused = false;
                } else {
                    room.zoomed = None;
                }
            }

    // `c`: conclude the selected context (fire-and-forget over RPC; the
            // next DriftState poll seats it in Bumped, never Recent — see
            // `assign_placement`).
            Action::Conclude => {
                if let Some(id) = state.selected
                    && let Some(actor) = actor.as_ref()
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

            // `p` / `d` / `z` / `a`: promote / demote / toggle-pause /
            // archive the selection — fire-and-forget over RPC, same pattern
            // as `c` above. The kernel owns the demote ladder and the
            // active-ring cap; a failure logs a visible warning with the
            // context short id. Promote's ring-full refusal ("active ring
            // full (10 seats) — demote something first") surfaces through
            // that same warn — seats never appear or vanish silently.
            //
            // Each verb passes the `begin_placement` in-flight guard first: a
            // second placement verb on the same context before the next poll
            // refreshes is ignored, so a double-`d` can't walk the ladder two
            // rungs sight-unseen (auto → demoted → ARCHIVED inside one poll
            // interval).
            Action::Promote => {
                if let Some(id) = state.selected
                    && let Some(actor) = actor.as_ref()
                {
                    if !state.begin_placement(id) {
                        info!(
                            "well: promote {} ignored — placement already in flight",
                            id.short()
                        );
                        continue;
                    }
                    let handle = actor.handle.clone();
                    bevy::tasks::IoTaskPool::get()
                        .spawn(async move {
                            if let Err(e) = handle.promote_context(id).await {
                                log::warn!("well: promote {} failed: {e}", id.short());
                            }
                        })
                        .detach();
                    info!("well: promote {}", id.short());
                }
            }

            Action::Demote => {
                if let Some(id) = state.selected
                    && let Some(actor) = actor.as_ref()
                {
                    if !state.begin_placement(id) {
                        info!(
                            "well: demote {} ignored — placement already in flight",
                            id.short()
                        );
                        continue;
                    }
                    let handle = actor.handle.clone();
                    bevy::tasks::IoTaskPool::get()
                        .spawn(async move {
                            if let Err(e) = handle.demote_context(id).await {
                                log::warn!("well: demote {} failed: {e}", id.short());
                            }
                        })
                        .detach();
                    info!("well: demote {}", id.short());
                }
            }

            // `z` toggles `paused_at`: the RPC takes the new absolute value,
            // so read the selection's current flag off the same poll
            // `sync.rs` reads (`DriftState`'s ContextInfo) and send its
            // negation. The in-flight guard matters here too — the flag read
            // is stale until the poll lands, so an unguarded double-`z` would
            // send the same value twice instead of toggling back.
            Action::PauseToggle => {
                if let Some(id) = state.selected
                    && let Some(actor) = actor.as_ref()
                {
                    if !state.begin_placement(id) {
                        info!(
                            "well: pause-toggle {} ignored — placement already in flight",
                            id.short()
                        );
                        continue;
                    }
                    let currently_paused = drift
                        .contexts
                        .iter()
                        .any(|c| c.id == id && c.paused_at.is_some());
                    let next_paused = !currently_paused;
                    let handle = actor.handle.clone();
                    bevy::tasks::IoTaskPool::get()
                        .spawn(async move {
                            if let Err(e) = handle.set_context_paused(id, next_paused).await {
                                log::warn!(
                                    "well: pause {} -> {next_paused} failed: {e}",
                                    id.short()
                                );
                            }
                        })
                        .detach();
                    info!("well: pause {} -> {next_paused}", id.short());
                }
            }

            Action::Archive => {
                if let Some(id) = state.selected
                    && let Some(actor) = actor.as_ref()
                {
                    if !state.begin_placement(id) {
                        info!(
                            "well: archive {} ignored — placement already in flight",
                            id.short()
                        );
                        continue;
                    }
                    let handle = actor.handle.clone();
                    bevy::tasks::IoTaskPool::get()
                        .spawn(async move {
                            if let Err(e) = handle.archive_context(id).await {
                                log::warn!("well: archive {} failed: {e}", id.short());
                            }
                        })
                        .detach();
                    info!("well: archive {}", id.short());
                }
            }

            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The in-flight guard is per-context: the first placement on an id wins,
    /// repeats are refused until the set clears (the next poll), and a
    /// different context is unaffected.
    #[test]
    fn begin_placement_guards_per_context_until_cleared() {
        let mut state = TimeWellState::default();
        let a = ContextId::new();
        let b = ContextId::new();

        assert!(state.begin_placement(a), "first verb on a context fires");
        assert!(!state.begin_placement(a), "repeat before the poll is refused");
        assert!(state.begin_placement(b), "another context is unaffected");

        // The poll landing clears the set (see sync_time_well) — everything
        // fires again.
        state.placement_pending.clear();
        assert!(state.begin_placement(a));
    }

    // -- placement (Slice B seam) — mirrors patch bay's `placement_*` tests --

    #[test]
    fn identity_placement_transform_is_the_identity() {
        let tf = placement_transform(&IDENTITY_PLACEMENT);
        assert_eq!(tf.translation, Vec3::ZERO);
        assert_eq!(tf.rotation, Quat::IDENTITY);
        assert_eq!(tf.scale, Vec3::ONE);
    }

    #[test]
    fn identity_placement_to_room_is_a_no_op() {
        // No visual change at Slice B: mapping any well-local point through
        // the identity placement must return that exact point.
        let p = Vec3::new(12.0, -34.0, 56.0);
        assert_eq!(placement_to_room(&IDENTITY_PLACEMENT, p), p);
    }

    // -- STATION_CENTER_PLACEMENT (Slice C's production placement) --

    #[test]
    fn station_center_placement_scale_shrinks_the_mouth_not_grows_it() {
        // Deliberately NOT pinned to a specific value — `STATION_CENTER_SCALE`
        // is an explicit live-tuning knob (its own doc has the retune
        // history) and a test asserting one exact number just breaks on
        // every retune without protecting against anything real. The actual
        // invariant: positive (a zero/negative scale would collapse or
        // invert the well's geometry) and shrinking the well's native
        // ~500-unit mouth radius, not growing past it.
        assert!(STATION_CENTER_PLACEMENT.scale > 0.0, "scale must be positive");
        assert!(STATION_CENTER_PLACEMENT.scale < 1.0, "scale should shrink the well, not grow it");
    }

    #[test]
    fn station_center_placement_seats_above_the_room_table() {
        // Rotation counters the well's own baked-in recline (`card::WELL_TILT`)
        // so the net X rotation lands at exactly -FRAC_PI_2 (Amy, live: "the
        // magic rings can be parallel to the floor") — assert the COMPOSED
        // angle, not identity, since the placement's own rotation is only half
        // of what the deck/rings actually end up rotated by.
        let net_x_angle = STATION_CENTER_PLACEMENT.rotation.to_euler(EulerRot::XYZ).0
            + super::super::card::WELL_TILT;
        assert!(
            (net_x_angle - (-std::f32::consts::FRAC_PI_2)).abs() < 1e-5,
            "net rotation (placement + the well's own tilt) should land the deck flat: {net_x_angle}"
        );
        assert_eq!(STATION_CENTER_PLACEMENT.translation.x, 0.0);
        assert_eq!(STATION_CENTER_PLACEMENT.translation.z, 0.0);
        assert!(
            STATION_CENTER_PLACEMENT.translation.y > crate::view::room::TABLE_TOP_Y,
            "the placement must lift the well above the table's own top face, not just to it"
        );
    }

    // -- well_zoomed (Slice C's dived-only run_if gate) --

    #[test]
    fn well_zoomed_true_only_when_the_room_is_zoomed_onto_the_well() {
        let mut room = crate::view::room::RoomState::default();
        assert!(!well_zoomed(&room), "unzoomed room is not well-zoomed");
        room.zoomed = Some(crate::view::room::nav::Station::PatchBay);
        assert!(!well_zoomed(&room), "zoomed on a different station is not well-zoomed");
        room.zoomed = Some(crate::view::room::nav::Station::TimeWell);
        assert!(well_zoomed(&room), "zoomed on TimeWell IS well-zoomed");
    }

    // -- arm_well / arm_dive reset semantics (mirrors patch_bay's `arm_scene`) --

    /// A minimal `ContextInfo` sufficient to seat a join entry — the rest of
    /// the fields don't matter for these reset-semantics tests.
    fn minimal_ctx(id: ContextId) -> kaijutsu_client::ContextInfo {
        kaijutsu_client::ContextInfo {
            id,
            label: "x".into(),
            forked_from: None,
            provider: String::new(),
            model: String::new(),
            created_at: 0,
            trace_id: [0u8; 16],
            fork_kind: None,
            context_type: String::new(),
            archived: false,
            concluded_at: None,
            keywords: Vec::new(),
            top_block_preview: None,
            live_status: kaijutsu_types::Status::Pending,
            last_activity_at: None,
            track_id: None,
            promoted_at: None,
            demoted_at: None,
            paused_at: None,
        }
    }

    /// The shape `TimeWellState`/`WellTracks` are left in after a room visit:
    /// both resources survive `RoomRoot`'s despawn, but their id→entity maps
    /// now dangle (the real entities died with it) and nothing has re-armed
    /// them for the next `enter_room` yet.
    fn persisted_after_a_room_visit() -> (TimeWellState, super::super::rays::WellTracks) {
        let mut state = TimeWellState::default();
        let id = ContextId::new();
        state.entities.insert(id, Entity::PLACEHOLDER);
        state.join.reconcile(std::iter::once((id, minimal_ctx(id))));
        state.last_seen_visible_count = 3;
        (state, super::super::rays::WellTracks::default())
    }

    #[test]
    fn arm_well_clears_the_dangling_entity_map_and_join() {
        let (mut state, mut tracks) = persisted_after_a_room_visit();
        assert!(!state.entities.is_empty());
        assert_eq!(state.join.len(), 1);

        arm_well(&mut state, &mut tracks);

        assert!(state.entities.is_empty(), "the stale entity map must clear");
        assert_eq!(state.join.len(), 0, "the join resets so sync_time_well rebuilds from scratch");
        assert_eq!(
            state.last_seen_visible_count, 0,
            "forces sync_time_well to re-run even if the visible count hasn't changed"
        );
    }

    #[test]
    fn arm_well_leaves_ring_centric_nav_state_untouched() {
        // Deliberately narrow (this function's own doc): ring focus/position
        // isn't tied to any entity, so it should survive a room round-trip
        // the same way `arm_dive` leaves ring rotation alone.
        let (mut state, mut tracks) = persisted_after_a_room_visit();
        state.focused_ring = 2;
        state.ring_pos = 5;
        let before = (state.focused_ring, state.ring_pos);

        arm_well(&mut state, &mut tracks);

        assert_eq!((state.focused_ring, state.ring_pos), before);
    }

    #[test]
    fn arm_dive_resets_focus_and_placement_pending_but_not_ring_rotation() {
        let mut state = TimeWellState::default();
        state.focused = true;
        state.placement_pending.insert(ContextId::new());
        state.ring_rotation[0] = 1.23;
        state.ring_rotation_target[0] = 4.56;
        state.card_text_dirty = false;

        arm_dive(&mut state);

        assert!(!state.focused, "a fresh dive starts at the ring overview, not mid-focus");
        assert!(state.placement_pending.is_empty(), "a stale in-flight guard must not block this dive");
        assert_eq!(state.ring_rotation[0], 1.23, "ring position persists across zoom out/in");
        assert_eq!(
            state.ring_rotation_target[0], 4.56,
            "the target must NOT be reset alone — an unpaired reset would spin the ring on every dive"
        );
        assert!(state.card_text_dirty, "arm_dive must arm a card-text rebuild for the dived-only text system");
    }

    #[test]
    fn arm_dive_resets_hero_so_a_re_dive_never_starts_stuck_in_the_hero_pose() {
        // Found by a kaibo review round, 2026-07-11: leaving a stale `hero`
        // unset here means Esc-then-re-dive lands back in the elevated
        // establishing shot, which ignores all input but Down/Esc, until the
        // user presses Down once to escape it — a stuck-state bug, not a
        // crash, but a real one. Unlike ring position/rotation, hero has no
        // visual anchor a user would recognize as "where I left off," so
        // every fresh dive should start OUT of it.
        let mut state = TimeWellState::default();
        state.hero = true;

        arm_dive(&mut state);

        assert!(!state.hero, "a fresh dive must never start parked in the hero pose");
    }

    // -- freeze-fix slice (2026-07-11): zoom-gated overlay helpers --

    #[test]
    fn ring_dim_factor_is_full_bright_everywhere_at_room_scale() {
        // Room scale has no "focused ring" — every band, whatever
        // `focused_ring` happens to hold, reads full brightness.
        for band in 0..super::super::card::N_BANDS {
            assert_eq!(
                ring_dim_factor(false, 2, band),
                1.0,
                "band {band} must be full-bright when unzoomed"
            );
        }
    }

    #[test]
    fn ring_dim_factor_dims_only_the_unfocused_bands_when_zoomed() {
        let focused = 1;
        assert_eq!(ring_dim_factor(true, focused, focused), 1.0, "the focused band stays full-bright");
        assert_eq!(ring_dim_factor(true, focused, 0), DIM_NONFOCUSED, "a different band dims");
        assert_eq!(ring_dim_factor(true, focused, 3), DIM_NONFOCUSED, "and another");
    }

    #[test]
    fn effective_selection_passes_through_while_zoomed() {
        let id = ContextId::new();
        assert_eq!(effective_selection(true, Some(id)), Some(id), "zoomed: the real selection shows");
        assert_eq!(effective_selection(true, None), None, "zoomed with nothing selected stays None");
    }

    #[test]
    fn effective_selection_is_none_at_room_scale_even_with_a_persisted_selection() {
        // The freeze this slice fixes: `state.selected` is deliberately left
        // set across a zoom-out (so a re-dive re-pops the same card — see
        // `arm_dive`'s own doc), but the overlays must not treat it as active
        // while unzoomed.
        let id = ContextId::new();
        assert_eq!(
            effective_selection(false, Some(id)),
            None,
            "a persisted selection must not leak into room-scale overlays"
        );
    }

    #[test]
    fn selection_scale_target_pops_only_when_selected() {
        assert_eq!(selection_scale_target(2.0, true), 2.0 * 1.35, "selected pops by 1.35x its base");
        assert_eq!(selection_scale_target(2.0, false), 2.0, "unselected settles at its own base scale");
    }
}

// ============================================================================
// PER-FRAME SYSTEMS
// ============================================================================

/// The selection the overlays should treat as active, folding the well's zoom
/// gate into it (freeze-fix slice, 2026-07-11): at room scale (`!zoomed`)
/// there is no selection pop/lineage highlight — `None` regardless of
/// `state.selected`. `state.selected` itself is left untouched by every
/// caller — it persists across the zoom so a re-dive re-pops the same card
/// (see `arm_dive`'s own doc + tests: nav state deliberately survives a room
/// round-trip). Shared by [`highlight_selection`], [`highlight_lineage`], and
/// [`super::drape::sync_lineage_drapes`] (`pub(super)` for that third,
/// sibling-module caller — same zoom-gate reasoning applies to the drapes as
/// to the ring highlight they were derived from).
pub(super) fn effective_selection(zoomed: bool, selected: Option<ContextId>) -> Option<ContextId> {
    if zoomed { selected } else { None }
}

/// Target `Transform.scale` for a rim card given its *effective* selection —
/// popped by 1.35× when selected, its own spiral base size otherwise.
fn selection_scale_target(base: f32, is_selected: bool) -> f32 {
    if is_selected { base * 1.35 } else { base }
}

/// Scale each card toward its spiral base size (set in `sync` from
/// [`super::card::spiral_scale`] — full at the mouth, shrinking toward the
/// throat), popping the selected one. The `selected` flag is written only when it
/// flips so the selection ring rebuilds on select/deselect without re-rasterizing
/// every frame; the scale tween itself runs every frame (eased) for a soft feel.
///
/// Ambient, not dived-only (freeze-fix slice, 2026-07-11): must react to a
/// zoom-OUT too, not just zoom-in, or a card left popped/selected on the last
/// dived frame stays frozen that way at room scale. [`effective_selection`]
/// folds the zoom gate in — at room scale every card eases back to
/// unselected/base-scale, while `state.selected` itself is left untouched.
pub fn highlight_selection(
    room: Res<crate::view::room::RoomState>,
    state: Res<TimeWellState>,
    mut cards: Query<(&mut Card, &mut Transform)>,
) {
    let selected = effective_selection(well_zoomed(&room), state.selected);
    for (mut card, mut tf) in cards.iter_mut() {
        let is_sel = Some(card.context_id) == selected;

        // Ring flag: write through the `Mut` only on a real change so we don't
        // trip `Changed<Card>` (the scene-rebuild trigger) every frame.
        if card.selected != is_sel {
            card.selected = is_sel;
        }

        // Snap-and-hold: while easing, write the eased scale; once within
        // SETTLE_EPS snap to the exact target once, then stop writing so a static
        // selection no longer fires `Changed<Transform>` every frame.
        let target = selection_scale_target(card.base_scale, is_sel);
        let s = tf.scale.x;
        if (s - target).abs() > SETTLE_EPS {
            let eased = s + (target - s) * 0.25;
            let next = if (eased - target).abs() <= SETTLE_EPS { target } else { eased };
            tf.scale = Vec3::splat(next);
        } else if s != target {
            tf.scale = Vec3::splat(target);
        }
    }
}

/// On-demand lineage overlay: light up the fork-ancestry of the selection.
///
/// Walks the selected context's `forked_from` chain (via the join's
/// `ContextInfo`) and flags each ancestor card's `in_lineage`. Like
/// [`highlight_selection`], the flag is written only when it flips, so the
/// lineage ring rebuilds on select-change without re-rasterizing every frame.
/// Nothing selected → no lineage.
///
/// Ambient, not dived-only (freeze-fix slice, 2026-07-11): same reasoning as
/// [`highlight_selection`], and shares its [`effective_selection`] helper —
/// at room scale the lineage set is always empty, every `in_lineage` flag
/// clears, regardless of a persisted `state.selected`.
pub fn highlight_lineage(
    room: Res<crate::view::room::RoomState>,
    state: Res<TimeWellState>,
    mut cards: Query<&mut Card>,
) {
    use std::collections::HashSet;
    let lineage: HashSet<ContextId> = match effective_selection(well_zoomed(&room), state.selected) {
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
///
/// Ambient, moved alongside its four siblings above (freeze-fix slice,
/// 2026-07-11) — but deliberately carries **no zoom branch**, unlike them:
/// [`DriftState`] is polled ungated on every screen (`ui/drift.rs`'s own
/// plugin wiring), so a staged drift's shimmer at room scale is truthful live
/// info, not stale frozen state, and it clears naturally the moment the next
/// poll lands — nothing here can freeze.
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
/// activity rings — is the open browser space; the selected card's detail
/// reads off the rim card's own face (header/gist/tail band) instead. Also
/// sidesteps the blank-white empty-selection state, which only the focus card
/// ever showed.
///
/// Ambient, not dived-only (freeze-fix slice, 2026-07-11): visibility now
/// derives from BOTH `well_zoomed` and `state.focused` — belt-and-braces,
/// since `focused` shouldn't survive an unzoom on its own, but deriving from
/// the zoom gate too means a stale/leftover `focused` can never leak the
/// focus card into room scale.
pub fn sync_focus_card_visibility(
    room: Res<crate::view::room::RoomState>,
    state: Res<TimeWellState>,
    mut card: Query<&mut Visibility, With<ReadingCard>>,
) {
    let want = if well_zoomed(&room) && state.focused {
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

/// Show/hide the [`HorizonLabel`] with the well's zoom state — mirrors
/// `crate::view::patch_bay`'s `apply_patch_lod` exactly (the original pattern
/// both copied): ambient, not dived-only, so it reacts to a zoom-OUT too, not
/// just zoom-in. At room scale the in-world "+N" event-horizon count stands
/// as an unreadable dark chip floating over the well (live-observed,
/// freeze-fix slice, 2026-07-11) — hidden until the next dive. Change-guarded
/// like every other LOD gate here.
pub fn apply_horizon_label_lod(
    room: Res<crate::view::room::RoomState>,
    mut label: Query<&mut Visibility, With<HorizonLabel>>,
) {
    let want = if well_zoomed(&room) { Visibility::Inherited } else { Visibility::Hidden };
    if let Ok(mut vis) = label.single_mut()
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

/// The `RingActivity` decay tick alone — split out of the old
/// `tick_and_sync_rings` (Slice C, `lovely-swimming-prism.md`) so it can run
/// **fully ungated**, mirroring `live::ingest_live_events`'s own ungated
/// system. This must NOT freeze outside the room: a design-review correction
/// caught that putting this in the room-gated ambient tier alongside
/// [`accumulate_ring_activity`] would reintroduce the exact
/// frozen-then-flashes-bright-on-reentry bug the old `exit_time_well`'s
/// `RingActivity::default()` reset used to guard against — energy now decays
/// toward zero in real time while you're away (Conversation, or any other
/// screen), so by the time you return it's already calmed down instead of
/// snapping back to a stale, high value. [`sync_deck_material`] (ambient —
/// room-gated) is the half that actually WRITES the decayed value into the
/// deck's material; nothing to draw, nothing gated here.
pub fn tick_ring_activity(time: Res<Time>, mut activity: ResMut<super::activity::RingActivity>) {
    activity.tick(time.delta_secs());
}

/// Push the well's pulse into the deck material uniforms: the global
/// `energy` (ring brightness / flow / core spin), the beat envelope
/// (`energy.y` — the throat heartbeat from whatever track is rolling, see
/// [`super::live::WellBeats`]), and the packed ripple array
/// (`[cos, sin, age_norm, intensity]`, unused slots `intensity = 0`). The
/// visual-write half of the old `tick_and_sync_rings` — ambient (room-gated),
/// since there's nothing to draw outside the room; the decay tick itself
/// moved to [`tick_ring_activity`] (ungated).
pub fn sync_deck_material(
    activity: Res<super::activity::RingActivity>,
    beats: Res<super::live::WellBeats>,
    mut ring_materials: ResMut<Assets<crate::shaders::WellRingsMaterial>>,
    deck: Query<&MeshMaterial3d<crate::shaders::WellRingsMaterial>, With<WellRingsDeck>>,
) {
    let Ok(handle) = deck.single() else {
        return;
    };
    let Some(mat) = ring_materials.get_mut(&handle.0) else {
        return;
    };

    mat.energy.x = activity.energy;
    mat.energy.y = beats.global_envelope(std::time::Instant::now());

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

/// Billboard every card (and the in-world "+N" label — [`HorizonLabel`] rides
/// the same fully-billboarded path as [`ReadingCard`], no `Card`) to face the
/// camera. No built-in billboard in 0.18; this is the one-line `looking_at`
/// per card the design doc calls for.
///
/// Reads the shared room camera ([`crate::view::room::RoomCamera`]) — this
/// system is ambient, running at room scale, where the room (not a
/// well-specific camera marker) owns the shared `Camera3d`
/// (`view/room/shot.rs::RoomShot::WellOverview` is what actually frames the
/// well once it's room furniture, Slice C; `Screen::TimeWell` and its own
/// camera marker are gone entirely as of Slice D).
pub fn billboard_cards(
    camera: Query<&GlobalTransform, With<crate::view::room::RoomCamera>>,
    mut cards: Query<
        (&mut Transform, &GlobalTransform, Option<&Card>),
        Or<(With<Card>, With<ReadingCard>, With<HorizonLabel>)>,
    >,
) {
    let Ok(cam) = camera.single() else {
        return;
    };
    let cam_pos = cam.translation();
    for (mut tf, global, card) in cards.iter_mut() {
        // Orient the quad's visible (+Z) face toward the camera, keeping world-up
        // so text stays upright. `looking_at` points -Z at its target, so aim it
        // at the point opposite the camera (the quad mirror of the camera ray).
        //
        // Uses the card's WORLD position (`global.translation()`), NOT its local
        // `tf.translation` — the two only coincide while this entity's ancestor
        // chain carries an identity transform. Reparented under the well's
        // placement (Slice B), local and world diverge once that placement
        // stops being identity, and `cam_pos` (from the camera's
        // `GlobalTransform`) is always world-space — mixing the two here was a
        // real bug (`lovely-swimming-prism.md`, Slice B), invisible under
        // Slice B's own identity placement. The result is still written into
        // the LOCAL `Transform.rotation` (what actually renders under the
        // parent) — correct even under `STATION_CENTER_PLACEMENT`'s real
        // rotation (Slice C, no longer identity): every direction feeding
        // `billboard_rot` below is derived from `GlobalTransform`, so the
        // computed world-space orientation is right regardless of the
        // parent's rotation, and Bevy's own transform propagation composes
        // the placement's rotation back on top of whatever local rotation
        // gets written here. Confirmed by a kaibo review round, 2026-07-11,
        // after this comment's first draft under-claimed the fix (said it
        // only held for an identity placement).
        let world_pos = global.translation();
        let away = world_pos * 2.0 - cam_pos;
        let billboard_rot = Transform::from_translation(world_pos)
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
        //
        // Deliberately LOCAL (`tf.translation`, not `global`/world) below: this
        // is resolving the card's position back into the well's OWN tilted
        // funnel frame (`well_tilt_quat().inverse()`), never relating it to the
        // camera's world position — no frame-mixing, so reparenting under the
        // placement doesn't touch its correctness the way `billboard_rot` above
        // needed fixing.
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
        // rises (no double-reclining). Skipped entirely at full ring-align
        // (RING_ALIGN == 1.0 zeroes the term); `card_tilt` stays the documented knob.
        if RING_ALIGN < 1.0 {
            rot *= Quat::from_rotation_x(-card_tilt(card.data.band) * (1.0 - RING_ALIGN));
        }
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
    // Ease each ring's rotation toward its gate target; snap-and-hold when close.
    // Track which rings actually moved this frame so we only rewrite *their*
    // cards' targets — a settled ring leaves its cards alone (Deepseek: the
    // recompute is only needed while spinning).
    let mut active = [false; super::card::N_BANDS];
    for i in 0..super::card::N_BANDS {
        // An empty ring has no cards to move (and sync_time_well's
        // `flen.max(1)` only gives it a synthetic single-slot gate target) —
        // skip the easing so an empty ring doesn't spin every tick.
        let flen = state.ring_cards[i].len();
        if flen == 0 {
            continue;
        }
        let cur = state.ring_rotation[i];
        let tgt = state.ring_rotation_target[i];
        if cur != tgt {
            let eased = cur + (tgt - cur) * alpha;
            state.ring_rotation[i] = if (eased - tgt).abs() <= SETTLE_EPS { tgt } else { eased };
            active[i] = true;
        }
    }
    let rot = state.ring_rotation; // Copy [f32; N_BANDS]
    for (seat, mut target) in cards.iter_mut() {
        let b = seat.band.index();
        if active[b] {
            let r = rot[b];
            target.0 =
                super::card::ring_seat_rotated(seat.band, seat.within_index, seat.ring_len, r);
        }
    }
}

/// Target dim factor for a ring/card at band index `band`, given the well's
/// focus state and zoom — extracted so the room-scale/dived branch is
/// unit-testable (freeze-fix slice, 2026-07-11). At room scale (`!zoomed`)
/// there is no "focused ring" concept — every band reads full brightness, no
/// dimming. Dived, the focused band is full brightness and every other band
/// dims to [`DIM_NONFOCUSED`], same as before this slice.
fn ring_dim_factor(zoomed: bool, focused_ring: usize, band: usize) -> f32 {
    if !zoomed || band == focused_ring { 1.0 } else { DIM_NONFOCUSED }
}

/// Dim everything **not** on the focused ring so the focused ring pops. Eases
/// each `TerraceRing`'s material alpha and each rim `Card`'s color-dim toward its
/// target — full for the focused band, × [`DIM_NONFOCUSED`] otherwise — so
/// Up/Down fades between rings. Ring alpha resets from [`TERRACE_RING_ALPHA`]
/// each frame (no compounding). The focus [`ReadingCard`] + the transient
/// legend panel are not rim `Card`s, so they're untouched and stay full
/// brightness.
///
/// Ambient, not dived-only (freeze-fix slice, 2026-07-11): must react to a
/// zoom-OUT too, not just zoom-in, or whatever dim state was live on the last
/// dived frame stays frozen at room scale — see [`ring_dim_factor`] for the
/// room-scale branch (everything full-bright there; there is no focused ring
/// outside the dive).
pub fn dim_nonfocused_rings(
    time: Res<Time>,
    room: Res<crate::view::room::RoomState>,
    state: Res<TimeWellState>,
    mut ring_materials: ResMut<Assets<crate::shaders::TerraceRingMaterial>>,
    rings: Query<(&TerraceRing, &MeshMaterial3d<crate::shaders::TerraceRingMaterial>)>,
    mut card_materials: ResMut<Assets<crate::shaders::WellCardMaterial>>,
    cards: Query<(&Card, &MeshMaterial3d<crate::shaders::WellCardMaterial>)>,
) {
    let zoomed = well_zoomed(&room);
    let focused = state.focused_ring;
    let alpha = 1.0 - (-DIM_EASE_RATE * time.delta_secs()).exp();

    // Snap-and-hold everywhere: read the current value via the immutable `get`
    // (which does NOT dirty the asset) and only reach for `get_mut` — the
    // expensive re-extract — when the value still differs from its target. Once
    // eased within SETTLE_EPS, snap to the exact target once; thereafter the
    // `cur != target` check is false and the material is never touched again.

    // Rings: target = base alpha × the focus factor (reset each frame, no compound).
    for (ring, handle) in rings.iter() {
        let factor = ring_dim_factor(zoomed, focused, ring.0);
        let target = TERRACE_RING_ALPHA * factor;
        let Some(cur) = ring_materials.get(&handle.0).map(|m| m.color.w) else {
            continue;
        };
        if cur != target {
            let eased = cur + (target - cur) * alpha;
            let next = if (eased - target).abs() <= SETTLE_EPS { target } else { eased };
            if let Some(mat) = ring_materials.get_mut(&handle.0) {
                mat.color.w = next;
            }
        }
    }

    // Rim cards: dim the color (not alpha — the material is alpha-masked).
    for (card, handle) in cards.iter() {
        let target = ring_dim_factor(zoomed, focused, card.data.band.index());
        let Some(cur) = card_materials.get(&handle.0).map(|m| m.dim.x) else {
            continue;
        };
        if cur != target {
            let eased = cur + (target - cur) * alpha;
            let next = if (eased - target).abs() <= SETTLE_EPS { target } else { eased };
            if let Some(mat) = card_materials.get_mut(&handle.0) {
                mat.dim.x = next;
            }
        }
    }
}

/// Ease each card toward its [`CardTarget`] (exponential smoothing, frame-rate
/// independent). This is the whole "transition system" — Bevy's frame loop does
/// the work the DOM made D3 reimplement.
pub fn move_cards_toward_target(time: Res<Time>, mut cards: Query<(&mut Transform, &CardTarget)>) {
    let alpha = 1.0 - (-CARD_EASE_RATE * time.delta_secs()).exp();
    for (mut tf, target) in cards.iter_mut() {
        // Snap-and-hold: once within CARD_SETTLE_DIST_SQ, snap to the exact target
        // once (if not already there) and stop writing, so a card at rest no longer
        // fires `Changed<Transform>` every frame. Read via Deref (no change mark);
        // only the conditional assign marks it.
        if tf.translation.distance_squared(target.0) <= CARD_SETTLE_DIST_SQ {
            if tf.translation != target.0 {
                tf.translation = target.0;
            }
            continue;
        }
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
///
/// Color-space check (feat/scene-palette-migration survey flag): `accent_color`
/// builds an `Hsla` (`Color::hsl`, CSS-HSL semantics — sRGB-referenced), and
/// `.to_linear()` converts THAT to `LinearRgba` — so `c` here is already
/// linear, correct for `WellCardMaterial`'s uniform (the 3D scene lane wants
/// linear values, docs/color.md). Verified correct; no fix needed.
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

/// `WellCardMaterial.shape` for the [`HorizonLabel`] panel: its own texture
/// aspect, no border ring (the label has no body fill or frame — see its
/// `accent`/`border` at spawn — just the MSDF text sampled onto the quad).
fn label_shape() -> Vec4 {
    Vec4::new(LABEL_TEX_W / LABEL_TEX_H, 0.0, 0.0, 0.0)
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
/// The thin ±X/±Y side faces get **sentinel UVs** (−1) instead of Bevy's
/// default full 0..1 mapping: a squeezed texture sliver on an 8-unit edge read
/// as noise, so `well_card.wgsl` detects the sentinel and paints those faces
/// as the card's *cut edge* — the border/accent color, the slab edge as the
/// card's border by design.
///
/// The mesh origin is the **bottom edge** (vertices shifted +Y by half the
/// height), not the center: a card's transform sits on its ring line
/// (`card::ring_seat_rotated`) with local +Y up the funnel axis
/// (`billboard_cards`), so the card *stands on* the ring — and the selection
/// pop (`highlight_selection`'s scale tween) grows it upward off the ring
/// instead of through it.
fn card_block_mesh() -> Mesh {
    use bevy::mesh::VertexAttributeValues;
    let mut mesh = Mesh::from(Cuboid::new(CARD_WIDTH, CARD_HEIGHT, CARD_THICKNESS));
    if let Some(VertexAttributeValues::Float32x2(uvs)) = mesh.attribute_mut(Mesh::ATTRIBUTE_UV_0) {
        // Bevy's cuboid vertex order: front (+Z) 0..4, back (−Z) 4..8, then the
        // four side faces 8..24 (right/left/top/bottom).
        for (i, uv) in uvs.iter_mut().enumerate() {
            if i < 4 {
                // Front face: V-flip to the Rectangle convention (see above).
                uv[1] = 1.0 - uv[1];
            } else if i >= 8 {
                // Side faces: sentinel — the shader renders these as the cut edge.
                *uv = [-1.0, -1.0];
            }
        }
    }
    mesh.translated_by(Vec3::Y * (CARD_HEIGHT * 0.5))
}

/// Saturation/lightness for [`accent_color`]'s per-context hue — room-palette
/// re-skin (2026-07-11). The old `(0.55, 0.55)` put the max channel at ~0.80
/// and the min at ~0.30 for every hue: full-neon, "bright arbitrary rainbow,"
/// at odds with the room's LDR-at-rest jewel tones (`BRASS_HUE`'s max channel
/// 0.72, `VIOLET_THREAD`'s 0.75). These land the max channel at ~0.68 and the
/// min at ~0.22 for every hue — restrained enough to sit in that family, still
/// saturated enough that distinct context buckets stay visually distinct
/// (this only changes the S/L the hash plugs into `Color::hsl`, not the
/// FNV→hue hashing itself). Shared by the rim cards' body/track-hue border
/// (`accent_vec4`, [`super::live::sync_card_live_uniforms`]) and the track
/// rays (`super::rays::sync_track_rays`). **Amy-tunable.**
const ACCENT_SATURATION: f32 = 0.42;
const ACCENT_LIGHTNESS: f32 = 0.45;

pub fn accent_color(accent: &str) -> Color {
    // FNV-1a over the bytes → hue. Stable, dependency-free.
    let mut h: u32 = 2166136261;
    for b in accent.bytes() {
        h ^= b as u32;
        h = h.wrapping_mul(16777619);
    }
    let hue = (h % 360) as f32;
    Color::hsl(hue, ACCENT_SATURATION, ACCENT_LIGHTNESS)
}
