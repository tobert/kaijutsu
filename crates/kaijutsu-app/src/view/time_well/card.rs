//! Pure `ContextInfo` → card-model mapping, band assignment, layout, and the
//! `LayoutPos → Vec3` well-lift.
//!
//! This is the seam between the zero-dependency `kaijutsu-viz` substrate (scales,
//! join, layout — all pure 2D / no glam) and the Bevy app. Per the substrate
//! notes in `docs/timewell.md` (appendix), the lift to `glam::Vec3` lives
//! **here**, on the app side, so the substrate stays free of a `glam` version
//! lockstep.
//!
//! Everything in this module is pure (no Bevy `World`, no GPU) and unit-tested.
//! The Bevy systems in the sibling modules call these functions; the cards they
//! produce are written onto entity components by the join system.

use bevy::math::{Quat, Vec3};
use kaijutsu_client::ContextInfo;
use kaijutsu_types::ContextId;
use kaijutsu_viz::layout::{ALL_BANDS, Band, ContextLifecycle, WellPlacement, assign_ring_seats};


/// Card-model fields derived from a single [`ContextInfo`]. Pure data, no Bevy.
///
/// The join system writes one of these onto each card entity (as the
/// `Card` component in the sibling module). Re-deriving on every data tick is
/// cheap; nothing here triggers relayout.
#[derive(Debug, Clone, PartialEq)]
pub struct CardData {
    /// Display title — the context label, or the short id when unlabeled.
    pub title: String,
    /// Accent bucket key (`context_type`, e.g. "coder"/"default") used to pick a
    /// card accent color. Falls back to `provider` when `context_type` is empty.
    pub accent: String,
    /// Model badge text, "provider/model" (or just one side if the other is
    /// empty; empty string if both are).
    pub model_badge: String,
    /// Fork badge ("full"/"shallow"/"compact"/"subtree"), absent if not a fork.
    pub fork_badge: Option<String>,
    /// Synthesis keywords (may be empty).
    pub keywords: Vec<String>,
    /// Preview of the most representative block (absent if none).
    pub preview: Option<String>,
    /// Lifecycle band this card belongs to.
    pub band: Band,
    /// Parent context for lineage overlay (`None` for a root).
    pub forked_from: Option<ContextId>,
    /// Kernel-synthesized semantic-cluster label, set only for `Demoted` (ring
    /// 3, the deepest ring that still seats cards — nearest analog to the old
    /// haystack) cards that belong to a cluster. `None` for every shallower
    /// ring and for unclustered `Demoted` cards. (Semantic clustering proper
    /// is a Stage-3 concern; this mapping just keeps the one existing field
    /// meaningful under the explicit-placement scheme.)
    pub cluster_label: Option<String>,
    /// Whether `paused_at` is set. **Visuals only** — design-only state for
    /// now (see `ContextRow::paused_at` in kernel_db.rs): the card shows a
    /// paused marker and HUD North's status line says so, but nothing about
    /// placement or behavior changes.
    pub paused: bool,
}

/// A context's semantic-cluster assignment (from `get_clusters`): the cluster id
/// (drives haystack angular grouping) and the kernel-synthesized label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterAssignment {
    pub cluster_id: u32,
    pub label: String,
}

/// Build a [`CardData`] from a [`ContextInfo`] and its pre-assigned [`Band`].
///
/// Band is passed in (not derived here) because it depends on the *whole set* —
/// see [`assign_placement`]. Everything else is a per-context field map with the
/// fallbacks the design doc's card table specifies.
pub fn card_from(info: &ContextInfo, band: Band, cluster_label: Option<String>) -> CardData {
    let title = info.id.display_or(Some(info.label.as_str()));

    let accent = if info.context_type.is_empty() {
        info.provider.clone()
    } else {
        info.context_type.clone()
    };

    let model_badge = match (info.provider.is_empty(), info.model.is_empty()) {
        (true, true) => String::new(),
        (false, true) => info.provider.clone(),
        (true, false) => info.model.clone(),
        (false, false) => format!("{}/{}", info.provider, info.model),
    };

    // A fork badge only when there's a non-empty fork kind.
    let fork_badge = info.fork_kind.as_ref().filter(|k| !k.is_empty()).cloned();

    CardData {
        title,
        accent,
        model_badge,
        fork_badge,
        keywords: info.keywords.clone(),
        preview: info.top_block_preview.clone(),
        band,
        forked_from: info.forked_from,
        // Only Demoted cards carry a cluster label; the caller passes `None`
        // for shallower rings (their angle encodes a different axis).
        cluster_label: if band == Band::Demoted {
            cluster_label
        } else {
            None
        },
        paused: info.paused_at.is_some(),
    }
}

/// The activity timestamp a context's placement derives from: its
/// `last_activity_at` when the kernel has one, else `created_at` (a context
/// that has never been touched since creation is exactly as recent as its
/// birth). Single source so [`assign_placement`] never disagrees with itself.
fn effective_activity(info: &ContextInfo) -> u64 {
    info.last_activity_at.unwrap_or(info.created_at)
}

/// Each ring's seated context ids in **seat order**, indexed by [`Band::index`]
/// (`[Active, Recent, Bumped, Demoted]`). This is the single source of seat
/// order: the layout derives every position from it (so angle == seat), and
/// keyboard navigation walks the same vectors (so the keys match the visuals).
/// Never longer than [`kaijutsu_viz::layout::RING_SLOTS`] per ring.
pub type BandOrders = [Vec<ContextId>; 4];

/// Adapt the app's [`ContextInfo`] poll into [`kaijutsu_viz::layout::assign_ring_seats`]'s
/// pure `ContextLifecycle<ContextId>` model and run it: the one place that
/// maps wire/client fields onto the placement engine's inputs. Archived
/// contexts are expected to be filtered out *before* this call (the well
/// doesn't show them at all) — see `sync::sync_time_well`.
///
/// Replaces the old two-step `assign_bands` (per-context idle-age
/// classification) + `band_orders` (recency ordering within a band): seating
/// and ordering are now one whole-set computation, so there's one adapter
/// instead of two.
pub fn assign_placement(contexts: &[ContextInfo]) -> WellPlacement<ContextId> {
    let lifecycles: Vec<ContextLifecycle<ContextId>> = contexts
        .iter()
        .map(|c| ContextLifecycle {
            id: c.id,
            created_at: c.created_at as i64,
            concluded_at: c.concluded_at.map(|ts| ts as i64),
            last_activity_at: effective_activity(c) as i64,
            promoted_at: c.promoted_at.map(|ts| ts as i64),
            demoted_at: c.demoted_at.map(|ts| ts as i64),
        })
        .collect();

    assign_ring_seats(&lifecycles)
}

/// Dense, collision-free order keys that group same-cluster contexts angularly
/// adjacent.
///
/// **Not called by [`assign_placement`]** — recency (or the explicit
/// promote/demote stamp) orders every ring instead (see its doc). This is the
/// Stage-3 grouping primitive (`docs/timewell.md`, "Tracks on the wire, and in
/// the well"): kept live and reachable via its own tests below so it doesn't
/// bit-rot before its caller lands.
///
/// Contexts are ranked `0..n` after sorting by `(cluster_id, id)`, with
/// **unclustered** contexts (no entry in `cluster_of`) trailing after all
/// clusters (sorted among themselves by id). Deterministic: the keys depend
/// only on cluster membership and id, never on input order.
#[allow(dead_code)] // Stage-3 grouping primitive; not called until tracks land (see doc above)
pub fn haystack_order_keys(
    contexts: &[ContextId],
    cluster_of: &std::collections::HashMap<ContextId, ClusterAssignment>,
) -> std::collections::HashMap<ContextId, i64> {
    use std::cmp::Ordering;
    let mut sorted = contexts.to_vec();
    sorted.sort_by(|a, b| {
        let ca = cluster_of.get(a).map(|c| c.cluster_id);
        let cb = cluster_of.get(b).map(|c| c.cluster_id);
        match (ca, cb) {
            (Some(x), Some(y)) => x.cmp(&y).then_with(|| a.cmp(b)),
            (Some(_), None) => Ordering::Less, // clustered before unclustered
            (None, Some(_)) => Ordering::Greater,
            (None, None) => a.cmp(b),
        }
    });
    sorted
        .iter()
        .enumerate()
        .map(|(i, id)| (*id, i as i64))
        .collect()
}

/// Walk the fork lineage of `start` upward, returning the set of its ancestor
/// context ids (parents, grandparents, … to the root). Excludes `start` itself.
///
/// `parent_of` maps a context to its `forked_from` parent (`None` at a root). The
/// walk is cycle-safe — a parent already seen ends it — so a malformed lineage
/// can't hang the overlay. Drives the on-demand lineage highlight: select a card,
/// its ancestry lights up (the lineage overlay, `docs/timewell.md`).
pub fn ancestors(
    start: ContextId,
    parent_of: impl Fn(ContextId) -> Option<ContextId>,
) -> std::collections::HashSet<ContextId> {
    let mut out = std::collections::HashSet::new();
    let mut cur = parent_of(start);
    while let Some(p) = cur {
        if !out.insert(p) {
            break; // cycle guard
        }
        cur = parent_of(p);
    }
    out
}

/// Collect the context ids that are endpoints (source **or** target) of any
/// staged drift. A card whose context is in this set shimmers (the "drift =
/// shimmer" bling): something is staged to flow into or out of it. Pure over the
/// staged queue so it's unit-testable without the kernel.
pub fn drift_endpoints(
    staged: &[kaijutsu_client::StagedDriftInfo],
) -> std::collections::HashSet<ContextId> {
    let mut out = std::collections::HashSet::new();
    for d in staged {
        out.insert(d.source_ctx);
        out.insert(d.target_ctx);
    }
    out
}

/// Pitch (radians) the whole well is tipped back about its X (horizontal) axis.
/// Negative tips the throat **down-and-away** so the mouth opens toward the
/// camera (the well-axis recline we designed). The single knob for the recline.
pub const WELL_TILT: f32 = -0.95;

/// The well's recline as a rotation about +X by [`WELL_TILT`]. Applied to card
/// positions in [`spiral_pos`] and to the ring deck so both share one funnel axis.
pub fn well_tilt_quat() -> Quat {
    Quat::from_rotation_x(WELL_TILT)
}

// ============================================================================
// STACKED BAND RINGS (one magic-ring per explicit/automatic ring — cards seated on it)
// ============================================================================
//
// The well is a stack of concentric magic-circle rings, one per ring
// (`Active` → `Recent` → `Bumped` → `Demoted`), receding into the funnel by
// depth. Each ring's cards are seated **evenly around its ring**, on the ring
// line, like slides in a Kodak Carousel tray (see [`ring_seat`]). This
// SUPERSEDES the earlier spiral-within-terrace layout — cards moved from
// *between* rings to *on* rings. A card's position keys only on its
// `(band, within_index)` pair and the ring's card count, so appends spread
// evenly around a ring without reflowing another ring. See `docs/timewell.md`.

/// Outermost ring radius — the `Active` ring at the mouth. Expanded (was ~330)
/// so the well reads big. **Amy-tunable.**
const SPIRAL_R_MOUTH: f32 = 500.0;
/// Radius floor — the **mouth-open invariant**: no ring, in any band, shrinks
/// below this; the center stays reserved for the ring deck / accretion glow.
const SPIRAL_R_THROAT: f32 = 48.0;
/// Each deeper band's ring radius = the previous band's × this — a *modest*
/// per-band shrink so the rings nest/stack without collapsing toward the axis.
/// **Amy-tunable.**
const RING_RADIUS_STEP: f32 = 0.85;
/// Funnel-local depth (−Z) **step** per deeper band: `Active` sits at depth 0
/// (the mouth) and each colder band steps this much deeper, so the rings stack
/// as distinct planes (Up/Down will read as moving between them). **Amy-tunable.**
const RING_DEPTH_STEP: f32 = -230.0;
/// Per-within-index geometric decay retained for [`spiral_scale`] (the
/// within-band scale falloff); it no longer positions cards.
const SPIRAL_DECAY: f32 = 0.93;
/// Card scale floor at the deepest band (mouth cards are 1.0), used by
/// [`spiral_scale`]. Kept high so cards stay readable as they recede.
const SPIRAL_SCALE_THROAT: f32 = 0.52;

/// Number of bands / rings — one per [`Band`] variant.
const N_TERRACES: usize = ALL_BANDS.len();

/// Number of band rings, exposed for the ring-centric nav state (array sizes in
/// `scene::TimeWellState`). Same count as [`N_TERRACES`].
pub const N_BANDS: usize = ALL_BANDS.len();

/// The **gate** angle: the seat angle a ring is spun to so the selected card
/// sits at the front. `PI` = the world −X seat (screen-left) — the ring position
/// whose perpendicular slide, under the funnel tilt, turns its face down-and-toward
/// a front camera (so it reads face-on). The camera framing sits on this seat's
/// face normal, so whatever card spins here is centered and legible. **Amy-tunable.**
pub const GATE_ANGLE: f32 = std::f32::consts::PI;

/// The `(radius, depth)` of `band`'s ring, **funnel-local** (the
/// [`well_tilt_quat`] recline is applied later, in [`ring_seat`] and the scene
/// spawn). Radius shrinks modestly per deeper band ([`RING_RADIUS_STEP`],
/// floored at [`SPIRAL_R_THROAT`]); depth steps clearly deeper per band
/// ([`RING_DEPTH_STEP`], `Active` at depth 0). Single source of ring geometry
/// for both card seating ([`ring_seat`]) and the magic-circle ring visual
/// (`terrace_ring_material`/`terrace_ring.wgsl`).
pub fn band_ring(band: Band) -> (f32, f32) {
    let i = band.index() as i32;
    let radius = (SPIRAL_R_MOUTH * RING_RADIUS_STEP.powi(i)).max(SPIRAL_R_THROAT);
    let depth = RING_DEPTH_STEP * i as f32;
    (radius, depth)
}

/// Seat the card at `within_index` (of `band_count` total in its band) **evenly
/// around** its band's ring, on the ring line — like slides in a Kodak Carousel
/// tray. Angle 0 is `within_index` 0; cards fan by `TAU / band_count`.
///
/// **Where angles land:** the funnel recline [`WELL_TILT`] is a rotation about
/// the world **+X** axis, which leaves +X fixed, so the local `+X` seat (angle
/// 0, zero rotation) maps to world **+X** — the ring's rightmost point (3-o'clock
/// under the base camera, which looks down −Z). The projector **gate** is
/// [`GATE_ANGLE`] (currently `π` → world **−X**, screen-left), *not* angle 0: nav
/// spins each ring so its selected card lands there. This zero-rotation
/// [`ring_seat`] is the documented reference form; the live placement path is
/// [`ring_seat_rotated`], which adds the ring's eased spin toward the gate.
///
/// This is the zero-rotation convenience / documented reference form; the live
/// placement path uses [`ring_seat_rotated`] with the ring's eased spin.
#[allow(dead_code)] // documented reference form + test entry; runtime uses ring_seat_rotated
pub fn ring_seat(band: Band, within_index: usize, band_count: usize) -> Vec3 {
    ring_seat_rotated(band, within_index, band_count, 0.0)
}

/// [`ring_seat`] with a ring **rotation offset** (radians) added to the seat
/// angle — the projector "spin." Cards ease into the spun position because
/// `sync`/`spin_rings` recompute each card's `CardTarget` from this as the
/// ring's eased rotation advances (no new tween). `rotation = 0` reproduces
/// [`ring_seat`].
pub fn ring_seat_rotated(band: Band, within_index: usize, band_count: usize, rotation: f32) -> Vec3 {
    let (r, depth) = band_ring(band);
    let a = std::f32::consts::TAU * within_index as f32 / band_count.max(1) as f32 + rotation;
    well_tilt_quat() * Vec3::new(r * a.cos(), r * a.sin(), depth)
}

// ── Ring-centric navigation math (pure; unit-tested) ────────────────────────

/// The absolute ring rotation that seats the card at `ring_pos` (of `ring_len`)
/// exactly at [`GATE_ANGLE`]: solving `TAU·pos/len + rotation ≡ GATE_ANGLE`
/// gives `rotation = GATE_ANGLE − TAU·pos/len`. Empty ring → `GATE_ANGLE`.
pub fn gate_rotation(ring_pos: usize, ring_len: usize) -> f32 {
    if ring_len == 0 {
        return GATE_ANGLE;
    }
    GATE_ANGLE - std::f32::consts::TAU * ring_pos as f32 / ring_len as f32
}

/// Shortest signed delta (in `(-PI, PI]`) to turn from `current` to any angle
/// congruent to `target` modulo a full turn — so the ring spins the *short* way,
/// never multiple turns.
pub fn shortest_angle_delta(current: f32, target: f32) -> f32 {
    let d = (target - current).rem_euclid(std::f32::consts::TAU);
    if d > std::f32::consts::PI {
        d - std::f32::consts::TAU
    } else {
        d
    }
}

/// New rotation **target** that spins `ring_len`'s `ring_pos` card to the gate,
/// the short way from `current_target`. Accumulates on the current target (not
/// re-wrapped) so repeated steps chain continuously; the *seat* angle is
/// periodic, so the unbounded value is harmless.
pub fn spin_target_to_gate(current_target: f32, ring_pos: usize, ring_len: usize) -> f32 {
    let desired = gate_rotation(ring_pos, ring_len);
    current_target + shortest_angle_delta(current_target, desired)
}

/// Step a within-ring position by `delta`, **wrapping** modulo `len` (Left/Right
/// walk the ring). Empty ring → 0.
pub fn step_ring_pos(pos: usize, len: usize, delta: i32) -> usize {
    if len == 0 {
        return 0;
    }
    let l = len as i32;
    (((pos as i32 + delta) % l + l) % l) as usize
}

/// Carry a within-ring position onto a ring of `new_len` (Up/Down keep the index,
/// clamped to the new ring's last slot). Empty target ring → 0.
pub fn carry_ring_pos(pos: usize, new_len: usize) -> usize {
    if new_len == 0 {
        0
    } else {
        pos.min(new_len - 1)
    }
}

/// One ring per band, in mouth→throat order (`Active` … `Demoted`), each a
/// funnel-local `(radius, depth)` via [`band_ring`]. The scene spawns one
/// magic-circle ring quad per entry (sized to its own radius); cards are seated
/// on each ring via [`ring_seat`]. Radii shrink and `|depth|` grows down the
/// list (the funnel narrows + recedes).
pub fn terrace_ring_geometry() -> Vec<(f32, f32)> {
    ALL_BANDS.iter().map(|&band| band_ring(band)).collect()
}

/// Per-card **within-terrace** scale at `(band, within_index)`: 1.0 at the
/// mouth, shrinking toward [`SPIRAL_SCALE_THROAT`] at the deepest terrace's
/// inner edge, same per-band envelope-division as [`terrace_envelope`] (no gap
/// needed — scale has no "visible step" requirement, just continuous
/// recession). This is the continuous decay *inside* a band; the per-band
/// terrace step ([`TERRACE_SCALE_STEP`]) is layered on top in
/// [`card_base_scale`], which is what a card's `base_scale` actually reads.
pub fn spiral_scale(band: Band, within_index: usize) -> f32 {
    let n = N_TERRACES as f32;
    let i = band.index() as f32;
    let scale_span = (1.0 - SPIRAL_SCALE_THROAT) / n;
    let scale_outer = 1.0 - i * scale_span;
    let scale_inner = scale_outer - scale_span;
    let f = SPIRAL_DECAY.powi(within_index as i32);
    scale_inner + (scale_outer - scale_inner) * f
}

/// Multiplicative per-band scale step: each deeper band's cards are this factor
/// smaller than the previous band's, layered on top of the within-terrace
/// [`spiral_scale`] decay so the terraces read as distinctly-sized tiers
/// (`Active` 1.0× → `Recent` 0.8× → `Bumped` 0.64× → `Demoted` 0.512×).
/// **Amy-tunable.**
pub const TERRACE_SCALE_STEP: f32 = 0.8;

/// The card's **base render scale** at `(band, within_index)` — the single
/// source for a card's `base_scale` field (the value the render-scale tween
/// reads). Combines the within-terrace [`spiral_scale`] decay with the
/// per-band [`TERRACE_SCALE_STEP`] tier step, so deeper bands are distinctly
/// smaller overall.
pub fn card_base_scale(band: Band, within_index: usize) -> f32 {
    spiral_scale(band, within_index) * TERRACE_SCALE_STEP.powi(band.index() as i32)
}

/// Per-context `(Band, within-ring seat index)`, alongside the flat
/// mouth→throat odometer order, derived from an already-seated [`BandOrders`]
/// (the [`assign_placement`] output's `rings`). Single source: [`spiral_order`]
/// derives its flat `Vec` from this; `sync.rs` resolves each card's terraced
/// position/scale (`ring_seat_rotated`/`card_base_scale`) from the `(band,
/// within_index)` pair here.
pub fn spiral_positions(
    rings: &BandOrders,
) -> (Vec<ContextId>, std::collections::HashMap<ContextId, (Band, usize)>) {
    let mut flat = Vec::new();
    let mut pos = std::collections::HashMap::new();
    for band in ALL_BANDS {
        for (within_index, &id) in rings[band.index()].iter().enumerate() {
            pos.insert(id, (band, within_index));
            flat.push(id);
        }
    }
    (flat, pos)
}

/// The whole well as one ordered spiral, **mouth → throat**: `Active` first,
/// then `Recent`, `Bumped`, `Demoted` — each ring in its own seat order (see
/// [`assign_placement`]). Not called by `sync.rs` today (ring-seat digit
/// addressing replaced the flat odometer — see `scene::well_keyboard`); kept
/// live and unit-tested as the simpler pure entry point for a future
/// flat-order-only caller.
#[allow(dead_code)] // superseded by ring-seat digit addressing; kept as a tested pure primitive
pub fn spiral_order(rings: &BandOrders) -> Vec<ContextId> {
    spiral_positions(rings).0
}

// ── Band labels (in-world terrace-edge labels; spawned by `scene::enter_time_well`,
// filled by `text::build_ring_labels`/`text::build_horizon_label`) ─────────

/// Amy-tunable: how far outside a band's ring a label parks, so it doesn't
/// collide with that ring's seated cards.
const LABEL_RADIUS_OFFSET: f32 = 40.0;

/// Amy-tunable: how far around the ring (radians) a label sits from
/// [`GATE_ANGLE`], so the shelf's nameplate parks *beside* the gate seat
/// rather than directly behind whatever card is eased to the gate.
const LABEL_GATE_OFFSET: f32 = 0.45;

/// World position for `band`'s floating label: parked just outside the band's
/// ring, angularly offset from the gate ([`GATE_ANGLE`] − [`LABEL_GATE_OFFSET`])
/// so the labels sit where the camera looks without hiding behind the selected
/// card — a terrace-edge nameplate per shelf. Same recline as everything else
/// ([`well_tilt_quat`]).
pub fn band_label_pos(band: Band) -> Vec3 {
    let (radius, depth) = band_ring(band);
    let a = GATE_ANGLE - LABEL_GATE_OFFSET;
    let r = radius + LABEL_RADIUS_OFFSET;
    let local = Vec3::new(r * a.cos(), r * a.sin(), depth);
    well_tilt_quat() * local
}

/// World position for the event-horizon "+N" label: parked at the funnel
/// **center** (radius 0 — no ring seats there) one more depth-step beyond the
/// deepest ring ([`Band::Demoted`]'s), same recline as everything else.
pub fn horizon_label_pos() -> Vec3 {
    let (demoted_radius, demoted_depth) = band_ring(Band::Demoted);
    // One more shelf past DEMOTED, in the same gate-side nameplate column as
    // `band_label_pos` (the funnel center reads right conceptually but sits
    // outside the gate-framed camera view).
    let a = GATE_ANGLE - LABEL_GATE_OFFSET;
    let r = demoted_radius + LABEL_RADIUS_OFFSET;
    let local = Vec3::new(r * a.cos(), r * a.sin(), demoted_depth + RING_DEPTH_STEP);
    well_tilt_quat() * local
}

/// Display text for a ring's terrace-edge label.
pub fn band_label_text(band: Band) -> &'static str {
    match band {
        Band::Active => "ACTIVE",
        Band::Recent => "RECENT",
        Band::Bumped => "BUMPED",
        Band::Demoted => "DEMOTED",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The one crate that sees both sides of the ring-cap constant pins them
    /// together: `kaijutsu_types::RING_SLOTS` is canonical (the kernel's
    /// `ACTIVE_RING_CAPACITY` derives from it), and zero-dep `kaijutsu_viz`
    /// carries its own literal. If this fails, fix viz's literal.
    #[test]
    fn viz_ring_slots_matches_the_canonical_types_constant() {
        assert_eq!(kaijutsu_viz::layout::RING_SLOTS, kaijutsu_types::RING_SLOTS);
    }

    /// Build a `ContextInfo` with the fields a card cares about; the rest default.
    fn ctx(id: ContextId, label: &str) -> ContextInfo {
        ContextInfo {
            id,
            label: label.to_string(),
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

    /// A deterministic ContextId from a single discriminant byte (UUIDv7-shaped:
    /// the leading bytes order the id, which is what the layout ranks on).
    fn id_of(n: u8) -> ContextId {
        let mut b = [0u8; 16];
        b[0] = n;
        ContextId::from_bytes(b)
    }

    fn staged(id: u64, source: ContextId, target: ContextId) -> kaijutsu_client::StagedDriftInfo {
        kaijutsu_client::StagedDriftInfo {
            id,
            source_ctx: source,
            target_ctx: target,
            content: String::new(),
            source_model: String::new(),
            drift_kind: kaijutsu_types::DriftKind::Push,
            created_at: 0,
        }
    }

    #[test]
    fn drift_endpoints_collects_both_ends() {
        let (a, b, c, d) = (id_of(1), id_of(2), id_of(3), id_of(4));
        let set = drift_endpoints(&[staged(1, a, b), staged(2, c, d)]);
        // Both source and target of every staged drift shimmer.
        assert_eq!(set.len(), 4);
        for id in [a, b, c, d] {
            assert!(set.contains(&id));
        }
        // A context with no staged drift does not.
        assert!(!set.contains(&id_of(9)));
    }

    #[test]
    fn drift_endpoints_empty_is_empty() {
        assert!(drift_endpoints(&[]).is_empty());
    }

    #[test]
    fn title_prefers_label_falls_back_to_short_id() {
        let id = id_of(1);
        let labeled = card_from(&ctx(id, "my work"), Band::Active, None);
        assert_eq!(labeled.title, "my work");

        let unlabeled = card_from(&ctx(id, ""), Band::Active, None);
        assert_eq!(unlabeled.title, id.short());
    }

    #[test]
    fn accent_is_context_type_then_provider() {
        let mut info = ctx(id_of(1), "x");
        info.context_type = "coder".to_string();
        info.provider = "anthropic".to_string();
        assert_eq!(card_from(&info, Band::Active, None).accent, "coder");

        info.context_type = String::new();
        assert_eq!(card_from(&info, Band::Active, None).accent, "anthropic");
    }

    #[test]
    fn model_badge_joins_provider_and_model() {
        let mut info = ctx(id_of(1), "x");
        info.provider = "anthropic".to_string();
        info.model = "claude-opus-4-8".to_string();
        assert_eq!(
            card_from(&info, Band::Active, None).model_badge,
            "anthropic/claude-opus-4-8"
        );

        info.model = String::new();
        assert_eq!(card_from(&info, Band::Active, None).model_badge, "anthropic");

        info.provider = String::new();
        assert_eq!(card_from(&info, Band::Active, None).model_badge, "");
    }

    #[test]
    fn fork_badge_present_only_for_nonempty_fork_kind() {
        let mut info = ctx(id_of(1), "x");
        assert_eq!(card_from(&info, Band::Active, None).fork_badge, None);

        info.fork_kind = Some(String::new());
        assert_eq!(card_from(&info, Band::Active, None).fork_badge, None);

        info.fork_kind = Some("subtree".to_string());
        assert_eq!(
            card_from(&info, Band::Active, None).fork_badge,
            Some("subtree".to_string())
        );
    }

    #[test]
    fn paused_reflects_paused_at() {
        let mut info = ctx(id_of(1), "x");
        assert!(!card_from(&info, Band::Active, None).paused, "no paused_at -> not paused");

        info.paused_at = Some(1);
        assert!(card_from(&info, Band::Active, None).paused, "paused_at set -> paused");
    }

    // `assign_placement` is a thin adapter over `kaijutsu_viz::layout::assign_ring_seats`
    // — the seating/ordering RULES (recency ranking, demoted/promoted ranking,
    // concluded-competes-only-in-Bumped, overflow-to-horizon, ties) are unit-
    // tested exhaustively in `kaijutsu-viz`. These tests only cover the
    // ADAPTER'S mapping: promoted_at/demoted_at plumbed through, and the
    // `effective_activity` fallback to `created_at`.

    #[test]
    fn assign_placement_seats_promoted_demoted_and_recent() {
        let mut promoted = ctx(id_of(1), "");
        promoted.promoted_at = Some(100);

        let mut demoted = ctx(id_of(2), "");
        demoted.demoted_at = Some(200);

        let mut recent = ctx(id_of(3), "");
        recent.last_activity_at = Some(9_000);

        let placement = assign_placement(&[promoted, demoted, recent]);
        assert_eq!(placement.rings[Band::Active.index()], vec![id_of(1)]);
        assert_eq!(placement.rings[Band::Demoted.index()], vec![id_of(2)]);
        assert_eq!(placement.rings[Band::Recent.index()], vec![id_of(3)]);
        assert!(placement.rings[Band::Bumped.index()].is_empty());
        assert!(placement.horizon.is_empty());
    }

    #[test]
    fn assign_placement_concluded_context_seats_in_bumped_not_recent() {
        let mut concluded = ctx(id_of(1), "");
        concluded.last_activity_at = Some(500);
        concluded.concluded_at = Some(500);
        let mut open = ctx(id_of(2), "");
        open.last_activity_at = Some(100);

        let placement = assign_placement(&[concluded, open]);
        assert_eq!(
            placement.rings[Band::Recent.index()],
            vec![id_of(2)],
            "concluded is excluded from Recent even though it's more recent"
        );
        assert_eq!(
            placement.rings[Band::Bumped.index()],
            vec![id_of(1)],
            "the concluded context seats in Bumped instead"
        );
    }

    #[test]
    fn assign_placement_coalesces_missing_last_activity_to_created_at() {
        // Two open contexts, neither ever touched: `created_at` breaks the
        // recency tie exactly like a real `last_activity_at` would.
        let mut older = ctx(id_of(1), "");
        older.created_at = 100;
        let mut newer = ctx(id_of(2), "");
        newer.created_at = 200;

        let placement = assign_placement(&[older, newer]);
        assert_eq!(
            placement.rings[Band::Recent.index()],
            vec![id_of(2), id_of(1)],
            "no last_activity_at falls back to created_at for recency ranking"
        );
    }

    #[test]
    fn band_ring_shrinks_radius_and_deepens_per_band() {
        let rings: Vec<(f32, f32)> = ALL_BANDS.iter().map(|&b| band_ring(b)).collect();
        // Active at the mouth: full radius, depth 0.
        assert!((rings[0].0 - SPIRAL_R_MOUTH).abs() < 1e-3, "mouth ring is the full radius");
        assert!(rings[0].1.abs() < 1e-3, "mouth ring sits at depth 0");
        // Radius shrinks and |depth| grows down the stack.
        for pair in rings.windows(2) {
            assert!(pair[0].0 > pair[1].0, "radius shrinks per deeper band: {} -> {}", pair[0].0, pair[1].0);
            assert!(pair[0].1.abs() < pair[1].1.abs(), "|depth| grows per deeper band");
        }
        // The radius floor holds for every band.
        for (r, _d) in &rings {
            assert!(*r >= SPIRAL_R_THROAT - 1e-3, "ring radius {r} stays above the floor");
        }
    }

    #[test]
    fn ring_seat_places_cards_evenly_on_the_band_ring() {
        let band = Band::Recent;
        let (r, _depth) = band_ring(band);
        let n = 6usize;
        // Undo the rigid funnel tilt to check the local ring radius + angle.
        let untilt = well_tilt_quat().inverse();
        for i in 0..n {
            let local = untilt * ring_seat(band, i, n);
            let lr = (local.x * local.x + local.y * local.y).sqrt();
            assert!((lr - r).abs() < 1e-2, "seat {i} local radius {lr} != ring radius {r}");
        }
        // Angle 0 (the gate) lands on local +X — the world +X the recline fixes.
        let seat0 = untilt * ring_seat(band, 0, n);
        assert!(seat0.x > 0.0 && seat0.y.abs() < 1e-3, "angle-0 seat sits on local +X (the gate)");
        // Seats are evenly spaced (consecutive angular gap ≈ TAU / n).
        let ang = |i: usize| {
            let l = untilt * ring_seat(band, i, n);
            l.y.atan2(l.x)
        };
        let step = ang(1) - ang(0);
        assert!((step - std::f32::consts::TAU / n as f32).abs() < 1e-3, "even angular spacing");
    }

    #[test]
    fn ring_seat_is_append_stable_and_deeper_bands_sit_lower() {
        // Position keys only on (band, within_index, band_count).
        assert_eq!(ring_seat(Band::Recent, 2, 5), ring_seat(Band::Recent, 2, 5));
        // After the recline the funnel depth maps mostly to world-Y, so a
        // deeper-band card sits lower in world space.
        let near = ring_seat(Band::Active, 0, 4);
        let far = ring_seat(Band::Demoted, 0, 4);
        assert!(far.y < near.y, "a deeper-band ring sits lower after the recline");
    }

    #[test]
    fn ring_seat_rotated_shifts_the_seat_angle_by_rotation() {
        use std::f32::consts::TAU;
        let untilt = well_tilt_quat().inverse();
        let base = untilt * ring_seat_rotated(Band::Active, 1, 6, 0.0);
        let spun = untilt * ring_seat_rotated(Band::Active, 1, 6, 0.3);
        let ang = |v: Vec3| v.y.atan2(v.x);
        let delta = (ang(spun) - ang(base)).rem_euclid(TAU);
        assert!((delta - 0.3).abs() < 1e-3, "rotation offset adds to the seat angle");
    }

    // ── Ring-centric nav math ────────────────────────────────────────────────

    #[test]
    fn spin_target_to_gate_lands_the_selected_card_on_the_gate() {
        use std::f32::consts::TAU;
        // For a spread of ring sizes, positions, and starting rotations, the
        // resulting seat angle of `ring_pos` must be congruent to GATE_ANGLE.
        for len in [1usize, 2, 3, 6, 12] {
            for pos in 0..len {
                for &start in &[0.0f32, 1.0, -2.5, 9.0] {
                    let tgt = spin_target_to_gate(start, pos, len);
                    let seat = TAU * pos as f32 / len as f32 + tgt;
                    let off = (seat - GATE_ANGLE).rem_euclid(TAU);
                    let off = off.min(TAU - off); // distance to 0 either way
                    assert!(off < 1e-3, "len {len} pos {pos} start {start}: seat off gate by {off}");
                }
            }
        }
    }

    #[test]
    fn spin_target_to_gate_takes_the_short_way() {
        // A one-slot step must never move more than half a turn.
        for len in [3usize, 6, 12, 100] {
            let t0 = spin_target_to_gate(0.0, 0, len);
            let t1 = spin_target_to_gate(t0, 1, len);
            assert!(
                (t1 - t0).abs() <= std::f32::consts::PI + 1e-4,
                "len {len}: one step moved {} (> PI, the long way)",
                (t1 - t0).abs()
            );
        }
    }

    #[test]
    fn shortest_angle_delta_is_bounded_and_correct() {
        use std::f32::consts::{PI, TAU};
        // Always within (-PI, PI].
        for &(c, t) in &[(0.0f32, 0.1), (0.0, 6.0), (3.0, -3.0), (-1.0, 4.0)] {
            let d = shortest_angle_delta(c, t);
            assert!(d > -PI - 1e-4 && d <= PI + 1e-4, "delta {d} out of (-PI, PI]");
            // c + d must be congruent to t.
            let off = (c + d - t).rem_euclid(TAU);
            assert!(off < 1e-3 || (TAU - off) < 1e-3, "c+d not congruent to t");
        }
    }

    #[test]
    fn step_ring_pos_wraps_both_ways() {
        assert_eq!(step_ring_pos(0, 5, 1), 1);
        assert_eq!(step_ring_pos(4, 5, 1), 0, "right wraps past the end");
        assert_eq!(step_ring_pos(0, 5, -1), 4, "left wraps before the start");
        assert_eq!(step_ring_pos(2, 5, -1), 1);
        assert_eq!(step_ring_pos(3, 0, 1), 0, "empty ring stays at 0");
    }

    #[test]
    fn carry_ring_pos_clamps_to_new_ring() {
        assert_eq!(carry_ring_pos(3, 5), 3, "fits → kept");
        assert_eq!(carry_ring_pos(7, 5), 4, "overflows → clamped to last slot");
        assert_eq!(carry_ring_pos(2, 0), 0, "empty target ring → 0");
    }

    #[test]
    fn spiral_scale_shrinks_to_a_floor() {
        assert!(
            spiral_scale(Band::Active, 0) > spiral_scale(Band::Demoted, 8),
            "cards shrink from mouth to the deepest terrace"
        );
        assert!(spiral_scale(Band::Active, 0) <= 1.0 + 1e-4, "mouth scale ~1.0");
        assert!(
            spiral_scale(Band::Demoted, 500) >= SPIRAL_SCALE_THROAT - 1e-4,
            "scale is floored at the throat"
        );
    }

    #[test]
    fn spiral_order_flattens_rings_mouth_to_throat() {
        // `spiral_order`/`spiral_positions` take an already-seated `BandOrders`
        // (`assign_placement`'s `rings` — seating and within-ring order are
        // decided upstream, tested in `kaijutsu-viz`); this only checks the
        // pure mouth→throat flatten, in given per-ring order, no re-sort.
        let rings: BandOrders = [
            vec![id_of(1)],
            vec![id_of(2), id_of(3)],
            vec![id_of(4)],
            vec![id_of(5)],
        ];
        let order = spiral_order(&rings);
        assert_eq!(order.first(), Some(&id_of(1)), "Active leads at the mouth");
        assert_eq!(order.last(), Some(&id_of(5)), "Demoted trails at the throat");
        assert_eq!(
            order,
            vec![id_of(1), id_of(2), id_of(3), id_of(4), id_of(5)],
            "Active -> Recent -> Bumped -> Demoted, each ring's own order preserved"
        );
    }

    fn cluster(id: u32, label: &str) -> ClusterAssignment {
        ClusterAssignment {
            cluster_id: id,
            label: label.to_string(),
        }
    }

    #[test]
    fn haystack_keys_group_same_cluster_adjacent() {
        use std::collections::HashMap;
        // ids 1,3 in cluster 7; ids 2,4 in cluster 2; id 5 unclustered.
        let mut map = HashMap::new();
        map.insert(id_of(1), cluster(7, "rust"));
        map.insert(id_of(3), cluster(7, "rust"));
        map.insert(id_of(2), cluster(2, "music"));
        map.insert(id_of(4), cluster(2, "music"));
        let ids: Vec<ContextId> = (1..=5u8).map(id_of).collect();

        let keys = haystack_order_keys(&ids, &map);

        // Order: cluster 2 (ids 2,4), then cluster 7 (ids 1,3), then unclustered (5).
        let mut by_rank: Vec<(i64, ContextId)> =
            keys.iter().map(|(id, k)| (*k, *id)).collect();
        by_rank.sort();
        let order: Vec<ContextId> = by_rank.into_iter().map(|(_, id)| id).collect();
        assert_eq!(
            order,
            vec![id_of(2), id_of(4), id_of(1), id_of(3), id_of(5)]
        );
        // Dense + collision-free.
        let mut ranks: Vec<i64> = keys.values().copied().collect();
        ranks.sort();
        assert_eq!(ranks, vec![0, 1, 2, 3, 4]);
    }

    #[test]
    fn haystack_keys_are_order_independent() {
        use std::collections::HashMap;
        let mut map = HashMap::new();
        map.insert(id_of(1), cluster(1, "a"));
        map.insert(id_of(2), cluster(1, "a"));
        let forward: Vec<ContextId> = vec![id_of(1), id_of(2), id_of(3)];
        let reverse: Vec<ContextId> = vec![id_of(3), id_of(2), id_of(1)];
        assert_eq!(
            haystack_order_keys(&forward, &map),
            haystack_order_keys(&reverse, &map),
            "haystack keys must depend only on (cluster, id), not input order"
        );
    }

    // `band_orders` (recency ranking within a band) no longer exists as its own
    // function — `assign_ring_seats` (kaijutsu-viz) decides seating AND
    // within-ring order together, and its recency-ranking rules are tested
    // exhaustively there. See `assign_placement_seats_promoted_demoted_and_recent`
    // above for this module's adapter-level coverage.

    #[test]
    fn ancestors_walks_fork_chain_to_root() {
        use std::collections::HashMap;
        // 4 → 3 → 2 → 1(root). parent_of(1) = None.
        let mut parent: HashMap<ContextId, ContextId> = HashMap::new();
        parent.insert(id_of(4), id_of(3));
        parent.insert(id_of(3), id_of(2));
        parent.insert(id_of(2), id_of(1));
        let lookup = |id: ContextId| parent.get(&id).copied();

        let anc = ancestors(id_of(4), lookup);
        assert_eq!(anc.len(), 3);
        for n in [1u8, 2, 3] {
            assert!(anc.contains(&id_of(n)), "missing ancestor {n}");
        }
        // `start` itself is not in its own ancestry.
        assert!(!anc.contains(&id_of(4)));
        // A root has no ancestors.
        assert!(ancestors(id_of(1), lookup).is_empty());
    }

    #[test]
    fn ancestors_is_cycle_safe() {
        use std::collections::HashMap;
        // Pathological cycle 1 → 2 → 1; the walk must terminate.
        let mut parent: HashMap<ContextId, ContextId> = HashMap::new();
        parent.insert(id_of(1), id_of(2));
        parent.insert(id_of(2), id_of(1));
        let anc = ancestors(id_of(1), |id| parent.get(&id).copied());
        // Both nodes seen once, then the cycle is cut.
        assert_eq!(anc.len(), 2);
    }

    #[test]
    fn cluster_label_set_only_for_demoted() {
        let info = ctx(id_of(1), "x");
        // Demoted card carries the label.
        let deep = card_from(&info, Band::Demoted, Some("rust".to_string()));
        assert_eq!(deep.cluster_label.as_deref(), Some("rust"));
        // Every shallower ring never carries a cluster label, even if one is passed.
        for band in [Band::Active, Band::Recent, Band::Bumped] {
            let c = card_from(&info, band, Some("rust".to_string()));
            assert_eq!(c.cluster_label, None, "{band:?} must not carry a cluster label");
        }
    }

    // ── Band label groundwork ────────────────────────────────────────────────

    #[test]
    fn band_label_text_is_distinct_per_band() {
        let labels: Vec<&str> = ALL_BANDS.iter().map(|&b| band_label_text(b)).collect();
        let mut unique = labels.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), labels.len(), "every band gets its own label text");
    }

    #[test]
    fn terrace_ring_geometry_is_one_ring_per_band() {
        let rings = terrace_ring_geometry();
        // One ring per band, in mouth→throat order.
        assert_eq!(rings.len(), ALL_BANDS.len(), "one ring per band");

        // Every radius sits within the mouth→throat span; each ring matches its
        // band's `band_ring`.
        for (i, &band) in ALL_BANDS.iter().enumerate() {
            let (radius, depth) = rings[i];
            assert_eq!((radius, depth), band_ring(band), "ring {i} == band_ring({band:?})");
            assert!(
                radius >= SPIRAL_R_THROAT - 1e-3 && radius <= SPIRAL_R_MOUTH + 1e-3,
                "ring {i} radius {radius} must sit within [{SPIRAL_R_THROAT}, {SPIRAL_R_MOUTH}]"
            );
        }

        // |depth| strictly increases per deeper band (the rings stack + recede).
        for pair in rings.windows(2) {
            assert!(
                pair[0].1.abs() < pair[1].1.abs(),
                "|depth| must strictly increase per deeper band: {} then {}",
                pair[0].1.abs(),
                pair[1].1.abs()
            );
        }
    }

    #[test]
    fn band_label_pos_recedes_deeper_per_band() {
        // Same recline as spiral_pos, so "deeper" reads as "lower Y" post-tilt.
        let positions: Vec<Vec3> = ALL_BANDS.iter().map(|&b| band_label_pos(b)).collect();
        for pair in positions.windows(2) {
            assert!(
                pair[1].y < pair[0].y,
                "each successive band's label must sit lower (deeper) than the last: {:?}",
                positions
            );
        }
    }

    #[test]
    fn horizon_label_sits_deeper_than_the_demoted_ring_label() {
        let demoted = band_label_pos(Band::Demoted);
        let horizon = horizon_label_pos();
        assert!(
            horizon.y < demoted.y,
            "the horizon label must sit lower (deeper) than Demoted's own label: {horizon:?} vs {demoted:?}"
        );
    }
}
