//! Pure math for the horizon dive: where a context sits, which card a
//! direction key snaps to, and which edges the current selection reveals.
//!
//! No Bevy `World`, no GPU, no query state — every function here is a
//! deterministic map from corpus data to geometry or graph structure, and is
//! unit-tested below. The scene module turns these numbers into entities.
//!
//! # The one invariant this module exists to protect
//!
//! **Position never depends on the query.** [`stream_coord`] takes no query
//! and no light; it is a function of `(family, id, fell_at, now)` only. That
//! is enforced by the type signature on purpose — spatial memory is the whole
//! reason to render a space at all instead of a list, and it dies the instant
//! a search re-flows the layout (`docs/horizon-dive.md`, "Query as light, not
//! as gravity"). Lights change brightness, scale, and where the camera looks;
//! they never move a card.

use bevy::math::{Vec2, Vec3};

use super::corpus::{DAY_MS, HorizonContext, unit_hash};

// ── The accretion stream ───────────────────────────────────────────────────

/// Ring radius at the mouth of the stream — just past the well's own deepest
/// ring, so the dive reads as *continuing* the funnel rather than cutting to
/// an unrelated space. **Amy-tunable.**
const STREAM_R_NEAR: f32 = 1150.0;
/// Ring radius at the far end. The stream narrows as it falls; things a year
/// gone crowd toward the axis.
const STREAM_R_FAR: f32 = 300.0;
/// Depth (world units along −Z) of the newest thing past the horizon.
const STREAM_DEPTH_NEAR: f32 = 0.0;
/// Depth of the oldest. Big: the dive wants a genuine sense of falling.
const STREAM_DEPTH_FAR: f32 = 7000.0;
/// Age (days) that maps to [`STREAM_DEPTH_FAR`]. Older clamps there rather
/// than running away — an archive has a floor, not an infinite tail.
const AGE_SPAN_DAYS: f32 = 365.0;
/// Angular half-width (radians) of a family's lane. A family is a *lane*, not
/// a line: members spread within it so a 40-context family doesn't render as
/// 40 cards at one bearing. ~11° each way. **Amy-tunable.**
const FAMILY_SPREAD: f32 = 0.17;
/// Radial jitter (world units) applied per context, so two same-family cards
/// that fell on the same day don't occupy the same point.
const RADIAL_JITTER: f32 = 175.0;
/// Depth jitter (world units), same reason on the other axis.
const DEPTH_JITTER: f32 = 165.0;

/// Salts for the three independent per-context draws.
const SALT_ANGLE: u64 = 0xA1;
const SALT_RADIUS: u64 = 0xB2;
const SALT_DEPTH: u64 = 0xC3;
/// Salt for the family's own bearing.
const SALT_FAMILY: u64 = 0xF0;

/// A context's seat in the accretion stream: polar within the stream's cross
/// section, plus how far it has fallen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct StreamCoord {
    /// Bearing (radians). Family lane + this context's spread within it.
    pub angle: f32,
    /// Distance from the stream axis.
    pub radius: f32,
    /// How far past the horizon, in world units. Grows with age.
    pub depth: f32,
}

/// Normalized age `0..1`: log-compressed so the crowded recent band gets most
/// of the depth budget and the sparse old tail doesn't waste it.
///
/// Ages before `fell_at` (a clock skew, or a stamp from the future) read as
/// `0` rather than going negative — the near edge of the stream, which is
/// where a just-fallen context belongs anyway.
pub fn age_fraction(fell_at_ms: i64, now_ms: i64) -> f32 {
    let age_days = ((now_ms - fell_at_ms).max(0) as f64 / DAY_MS as f64) as f32;
    let t = (1.0 + age_days).ln() / (1.0 + AGE_SPAN_DAYS).ln();
    t.clamp(0.0, 1.0)
}

/// Where a context sits. **Query-independent by construction** — see the
/// module doc.
///
/// `family` picks the angular lane (stable across restarts: it's an FNV hash
/// of the family root's id, the same recipe the well's track bearings use).
/// `id` picks this context's spread within that lane and its two jitters.
/// `fell_at_ms` vs `now_ms` picks the depth, and depth picks the radius (the
/// stream narrows as it falls).
pub fn stream_coord(family: u32, id: u32, fell_at_ms: i64, now_ms: i64) -> StreamCoord {
    let t = age_fraction(fell_at_ms, now_ms);
    let lane = std::f32::consts::TAU * unit_hash(family, SALT_FAMILY);
    let spread = FAMILY_SPREAD * (unit_hash(id, SALT_ANGLE) * 2.0 - 1.0);
    let radius = STREAM_R_NEAR + (STREAM_R_FAR - STREAM_R_NEAR) * t
        + RADIAL_JITTER * (unit_hash(id, SALT_RADIUS) * 2.0 - 1.0);
    let depth = STREAM_DEPTH_NEAR + (STREAM_DEPTH_FAR - STREAM_DEPTH_NEAR) * t
        + DEPTH_JITTER * (unit_hash(id, SALT_DEPTH) * 2.0 - 1.0);
    StreamCoord {
        angle: lane + spread,
        radius: radius.max(STREAM_R_FAR * 0.5),
        depth,
    }
}

/// Lift a [`StreamCoord`] into world space. The stream falls along −Z, the
/// same axis the well's funnel recedes on, so the dive's camera can keep the
/// well's "deeper is further away" reading.
pub fn stream_pos(c: StreamCoord) -> Vec3 {
    Vec3::new(c.radius * c.angle.cos(), c.radius * c.angle.sin(), -c.depth)
}

/// Convenience: a corpus record straight to a world position.
pub fn context_pos(c: &HorizonContext, now_ms: i64) -> Vec3 {
    stream_pos(stream_coord(c.family, c.id, c.fell_at_ms, now_ms))
}

// ── Snap navigation ────────────────────────────────────────────────────────

/// A directional nudge. There is no free flight and no cursor: a direction
/// key moves the *selection* to a neighbour, and the camera follows the
/// selection (principle 4, snap navigation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapDir {
    Left,
    Right,
    Up,
    Down,
}

impl SnapDir {
    /// Unit vector in **screen** space (y up — callers projecting through a
    /// Bevy viewport, whose y runs down, must flip before calling).
    pub fn axis(self) -> Vec2 {
        match self {
            SnapDir::Left => Vec2::NEG_X,
            SnapDir::Right => Vec2::X,
            SnapDir::Up => Vec2::Y,
            SnapDir::Down => Vec2::NEG_Y,
        }
    }
}

/// Half-angle of the acceptance cone, as `across/along` slope. `tan(60°)`:
/// generous, because a sparse lit set often has nothing within a tight cone
/// and a direction key that does nothing feels broken.
const CONE_SLOPE: f32 = 1.732;
/// How much off-axis distance costs relative to on-axis distance. > 1 so a
/// card straight ahead beats a slightly nearer one off to the side.
const OFF_AXIS_PENALTY: f32 = 1.6;

/// The candidate `dir` snaps to from `from`, or `None` if nothing lies that
/// way.
///
/// Screen-space, because "left" has to mean what the user sees, not what the
/// world axes say — the camera looks down the stream at an angle and the
/// world's +X is not the screen's. Candidates are `(index, screen_pos)`;
/// anything coincident with `from` is skipped.
///
/// Selection rule: inside a `±60°` cone around `dir`, minimise
/// `along + OFF_AXIS_PENALTY × across`. Ties break on the lower index so the
/// result is deterministic (a wobbling selection under identical geometry is
/// worse than an arbitrary-but-stable one).
pub fn snap_neighbor(from: Vec2, candidates: &[(usize, Vec2)], dir: SnapDir) -> Option<usize> {
    let axis = dir.axis();
    let perp = Vec2::new(-axis.y, axis.x);
    let mut best: Option<(f32, usize)> = None;
    for &(idx, pos) in candidates {
        let delta = pos - from;
        if delta.length_squared() < 1e-6 {
            continue;
        }
        let along = delta.dot(axis);
        if along <= 0.0 {
            continue;
        }
        let across = delta.dot(perp).abs();
        if across > along * CONE_SLOPE {
            continue;
        }
        let cost = along + OFF_AXIS_PENALTY * across;
        if best.is_none_or(|(bc, bi)| cost < bc || (cost == bc && idx < bi)) {
            best = Some((cost, idx));
        }
    }
    best.map(|(_, idx)| idx)
}

/// The set a direction key is allowed to land on: everything the query lit at
/// or above `threshold`, **plus** everything the current selection is linked
/// to, minus the selection itself.
///
/// The union is the point. Query-only navigation strands you when a
/// neighbour you can see (because it's on the constellation) isn't lit;
/// link-only navigation can't leave the local component. Together they give
/// "walk the results, or walk the graph" from the same four keys.
pub fn nav_candidates(
    selected: Option<usize>,
    lights: &[f32],
    threshold: f32,
    linked: &[Edge],
) -> Vec<usize> {
    let mut out: Vec<usize> = Vec::new();
    for (i, l) in lights.iter().enumerate() {
        if *l >= threshold && Some(i) != selected {
            out.push(i);
        }
    }
    for e in linked {
        let i = e.other as usize;
        if i < lights.len() && Some(i) != selected && !out.contains(&i) {
            out.push(i);
        }
    }
    out.sort_unstable();
    out
}

// ── Local edge reveal ──────────────────────────────────────────────────────

/// How a neighbour is related to the selection. Drives the constellation
/// line's colour; also its precedence when a context qualifies twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum EdgeKind {
    /// The selection's fork parent.
    Parent,
    /// A context forked from the selection.
    Child,
    /// Another child of the selection's parent.
    Sibling,
    /// A drift / crosstalk partner — the edges that make this a graph rather
    /// than a tree.
    Drift,
}

/// One revealed edge from the selection to a neighbour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Edge {
    /// The neighbour's corpus index.
    pub other: u32,
    pub kind: EdgeKind,
}

/// Cap on revealed edges. A context with 200 children would otherwise draw
/// the hairball this whole design exists to avoid; past the cap the extras
/// are simply not drawn (the reading line reports the count).
pub const MAX_EDGES: usize = 24;

/// The selection's immediate neighbourhood — **and nothing else** (principle
/// 3, local edge reveal: the full graph is never rendered).
///
/// Order is by [`EdgeKind`] precedence then by id, and a context that
/// qualifies under two kinds appears once, under the stronger one (a drift
/// partner that is also your child is drawn as a child). Deterministic, so
/// the constellation doesn't shuffle between frames.
pub fn neighborhood(selected: u32, corpus: &[HorizonContext]) -> Vec<Edge> {
    let Some(sel) = corpus.get(selected as usize) else {
        return Vec::new();
    };
    let mut edges: Vec<Edge> = Vec::new();
    let push = |edges: &mut Vec<Edge>, other: u32, kind: EdgeKind| {
        if other == selected || other as usize >= corpus.len() {
            return;
        }
        if edges.iter().any(|e| e.other == other) {
            return; // already claimed by a stronger kind
        }
        edges.push(Edge { other, kind });
    };

    if let Some(p) = sel.parent {
        push(&mut edges, p, EdgeKind::Parent);
    }
    for c in corpus.iter().filter(|c| c.parent == Some(selected)) {
        push(&mut edges, c.id, EdgeKind::Child);
    }
    if let Some(p) = sel.parent {
        for c in corpus.iter().filter(|c| c.parent == Some(p)) {
            push(&mut edges, c.id, EdgeKind::Sibling);
        }
    }
    for &d in &sel.drift {
        push(&mut edges, d, EdgeKind::Drift);
    }

    edges.sort_by_key(|e| (e.kind, e.other));
    edges.truncate(MAX_EDGES);
    edges
}

#[cfg(test)]
mod tests {
    use super::super::corpus::{HorizonState, synth_corpus};
    use super::*;

    const NOW: i64 = 1_800_000_000_000;

    fn node(id: u32, family: u32, parent: Option<u32>, age_days: f32) -> HorizonContext {
        HorizonContext {
            id,
            title: format!("n{id}"),
            accent: "coder".into(),
            keywords: Vec::new(),
            fell_at_ms: NOW - (age_days as f64 * DAY_MS as f64) as i64,
            parent,
            drift: Vec::new(),
            family,
            state: HorizonState::Overflow,
        }
    }

    // ── stream_coord ──

    #[test]
    fn stream_coord_is_deterministic() {
        let a = stream_coord(3, 17, NOW - 5 * DAY_MS, NOW);
        let b = stream_coord(3, 17, NOW - 5 * DAY_MS, NOW);
        assert_eq!(a, b, "the same context must land in the same seat every time");
    }

    #[test]
    fn depth_grows_with_age_and_saturates_past_the_span() {
        let d = |days: f32| stream_coord(1, 1, NOW - (days as f64 * DAY_MS as f64) as i64, NOW).depth;
        assert!(d(0.0) < d(1.0));
        assert!(d(1.0) < d(30.0));
        assert!(d(30.0) < d(365.0));
        // Past the span the depth stops running away.
        assert!(
            (d(365.0) - d(4000.0)).abs() < 1e-3,
            "beyond the span, depth clamps: {} vs {}",
            d(365.0),
            d(4000.0)
        );
    }

    #[test]
    fn a_future_stamp_reads_as_the_near_edge_not_a_negative_depth() {
        // Clock skew must not fling a card out in front of the camera.
        assert_eq!(age_fraction(NOW + 10 * DAY_MS, NOW), 0.0);
        let c = stream_coord(1, 1, NOW + 10 * DAY_MS, NOW);
        assert!(c.depth < STREAM_DEPTH_NEAR + DEPTH_JITTER + 1.0, "depth {} ran negative-ish", c.depth);
    }

    #[test]
    fn the_stream_narrows_as_it_falls() {
        // Compare the radius envelope (jitter-free) at the two ends by
        // averaging many ids, so a single unlucky jitter can't flip it.
        let mean_r = |days: f32| {
            let t: f32 = (0..64u32)
                .map(|i| stream_coord(1, i, NOW - (days as f64 * DAY_MS as f64) as i64, NOW).radius)
                .sum();
            t / 64.0
        };
        assert!(mean_r(0.0) > mean_r(365.0), "the far end must be tighter to the axis");
    }

    #[test]
    fn radius_never_collapses_onto_the_axis() {
        for id in 0..500u32 {
            let c = stream_coord(id % 9, id, NOW - 400 * DAY_MS, NOW);
            assert!(c.radius > 0.0, "id {id} radius {} collapsed", c.radius);
        }
    }

    #[test]
    fn a_family_shares_one_angular_lane() {
        // Every member of family 42 must sit within FAMILY_SPREAD of the
        // family bearing — that's what makes a lineage readable as a lane.
        let lane = std::f32::consts::TAU * unit_hash(42, SALT_FAMILY);
        for id in 0..200u32 {
            let c = stream_coord(42, id, NOW, NOW);
            let off = (c.angle - lane).abs();
            assert!(off <= FAMILY_SPREAD + 1e-5, "id {id} strayed {off} from its family lane");
        }
    }

    #[test]
    fn different_families_get_different_lanes() {
        let lanes: Vec<f32> = (0..40u32).map(|f| stream_coord(f, 0, NOW, NOW).angle).collect();
        let mut sorted = lanes.clone();
        sorted.sort_by(|a, b| a.partial_cmp(b).unwrap());
        sorted.dedup_by(|a, b| (*a - *b).abs() < 1e-6);
        assert_eq!(sorted.len(), lanes.len(), "family lanes collided");
    }

    #[test]
    fn same_family_same_day_contexts_do_not_share_a_point() {
        // The jitters exist precisely to stop this.
        let a = stream_pos(stream_coord(7, 100, NOW, NOW));
        let b = stream_pos(stream_coord(7, 101, NOW, NOW));
        assert!(a.distance(b) > 1.0, "two siblings landed on top of each other: {a:?} {b:?}");
    }

    #[test]
    fn positions_are_stable_across_a_whole_synthetic_corpus() {
        let corpus = synth_corpus(300, 5, NOW);
        let first: Vec<Vec3> = corpus.iter().map(|c| context_pos(c, NOW)).collect();
        let again: Vec<Vec3> = corpus.iter().map(|c| context_pos(c, NOW)).collect();
        assert_eq!(first, again);
    }

    // ── snap_neighbor ──

    #[test]
    fn snap_picks_the_nearest_candidate_in_the_direction() {
        let from = Vec2::ZERO;
        let cands = vec![
            (0, Vec2::new(100.0, 0.0)),
            (1, Vec2::new(40.0, 0.0)),
            (2, Vec2::new(-40.0, 0.0)),
            (3, Vec2::new(0.0, 60.0)),
        ];
        assert_eq!(snap_neighbor(from, &cands, SnapDir::Right), Some(1));
        assert_eq!(snap_neighbor(from, &cands, SnapDir::Left), Some(2));
        assert_eq!(snap_neighbor(from, &cands, SnapDir::Up), Some(3));
    }

    #[test]
    fn snap_returns_none_when_nothing_lies_that_way() {
        let cands = vec![(0, Vec2::new(100.0, 0.0))];
        assert_eq!(snap_neighbor(Vec2::ZERO, &cands, SnapDir::Down), None);
        assert_eq!(snap_neighbor(Vec2::ZERO, &[], SnapDir::Right), None);
    }

    #[test]
    fn snap_skips_a_candidate_sitting_on_the_cursor() {
        let cands = vec![(0, Vec2::ZERO), (1, Vec2::new(30.0, 0.0))];
        assert_eq!(snap_neighbor(Vec2::ZERO, &cands, SnapDir::Right), Some(1));
    }

    #[test]
    fn snap_rejects_candidates_outside_the_cone() {
        // 80° off-axis: visible, but not "to the right" by any honest reading.
        let cands = vec![(0, Vec2::new(20.0, 113.0))];
        assert_eq!(snap_neighbor(Vec2::ZERO, &cands, SnapDir::Right), None);
        assert_eq!(snap_neighbor(Vec2::ZERO, &cands, SnapDir::Up), Some(0));
    }

    #[test]
    fn snap_prefers_on_axis_over_a_slightly_nearer_off_axis_card() {
        // 0: 100px straight ahead. 1: 90px ahead but 70px off to the side.
        // cost(0) = 100; cost(1) = 90 + 1.6*70 = 202.
        let cands = vec![(0, Vec2::new(100.0, 0.0)), (1, Vec2::new(90.0, 70.0))];
        assert_eq!(snap_neighbor(Vec2::ZERO, &cands, SnapDir::Right), Some(0));
    }

    #[test]
    fn snap_ties_break_deterministically_on_the_lower_index() {
        let cands = vec![(5, Vec2::new(50.0, 0.0)), (2, Vec2::new(50.0, 0.0))];
        assert_eq!(snap_neighbor(Vec2::ZERO, &cands, SnapDir::Right), Some(2));
        let reversed = vec![(2, Vec2::new(50.0, 0.0)), (5, Vec2::new(50.0, 0.0))];
        assert_eq!(
            snap_neighbor(Vec2::ZERO, &reversed, SnapDir::Right),
            Some(2),
            "input order must not decide the winner"
        );
    }

    // ── nav_candidates ──

    #[test]
    fn nav_candidates_unions_lit_and_linked_minus_self() {
        let lights = vec![1.0, 0.0, 0.6, 0.0, 0.0];
        let linked = vec![Edge { other: 3, kind: EdgeKind::Child }];
        // selection 2 is lit but must not be its own neighbour.
        assert_eq!(nav_candidates(Some(2), &lights, 0.5, &linked), vec![0, 3]);
    }

    #[test]
    fn nav_candidates_ignores_links_past_the_end_of_the_corpus() {
        let lights = vec![1.0, 0.0];
        let linked = vec![Edge { other: 99, kind: EdgeKind::Drift }];
        assert_eq!(nav_candidates(Some(0), &lights, 0.5, &linked), Vec::<usize>::new());
    }

    #[test]
    fn nav_candidates_with_no_selection_keeps_every_lit_card() {
        let lights = vec![1.0, 0.2, 0.9];
        assert_eq!(nav_candidates(None, &lights, 0.5, &[]), vec![0, 2]);
    }

    // ── neighborhood ──

    #[test]
    fn neighborhood_finds_parent_children_siblings_and_drift() {
        //   0 ── 1 (selected) ── 3
        //     └─ 2 (sibling)
        //   1 ↔ 4 (drift, other family)
        let mut corpus = vec![
            node(0, 0, None, 1.0),
            node(1, 0, Some(0), 1.0),
            node(2, 0, Some(0), 1.0),
            node(3, 0, Some(1), 1.0),
            node(4, 9, None, 1.0),
        ];
        corpus[1].drift.push(4);
        corpus[4].drift.push(1);

        let edges = neighborhood(1, &corpus);
        let kind_of = |id: u32| edges.iter().find(|e| e.other == id).map(|e| e.kind);
        assert_eq!(kind_of(0), Some(EdgeKind::Parent));
        assert_eq!(kind_of(3), Some(EdgeKind::Child));
        assert_eq!(kind_of(2), Some(EdgeKind::Sibling));
        assert_eq!(kind_of(4), Some(EdgeKind::Drift));
        assert_eq!(edges.len(), 4, "and nothing else — local reveal only");
        assert!(!edges.iter().any(|e| e.other == 1), "never an edge to itself");
    }

    #[test]
    fn neighborhood_is_ordered_by_kind_precedence_then_id() {
        let corpus = vec![
            node(0, 0, None, 1.0),
            node(1, 0, Some(0), 1.0),
            node(2, 0, Some(0), 1.0),
            node(3, 0, Some(1), 1.0),
            node(4, 0, Some(1), 1.0),
        ];
        let edges = neighborhood(1, &corpus);
        let seq: Vec<(EdgeKind, u32)> = edges.iter().map(|e| (e.kind, e.other)).collect();
        assert_eq!(
            seq,
            vec![
                (EdgeKind::Parent, 0),
                (EdgeKind::Child, 3),
                (EdgeKind::Child, 4),
                (EdgeKind::Sibling, 2),
            ]
        );
    }

    #[test]
    fn a_context_qualifying_twice_appears_once_under_the_stronger_kind() {
        // 2 is both a child of 1 and a drift partner of 1 — draw it as a child.
        let mut corpus = vec![node(0, 0, None, 1.0), node(1, 0, Some(0), 1.0), node(2, 0, Some(1), 1.0)];
        corpus[1].drift.push(2);
        let edges = neighborhood(1, &corpus);
        let hits: Vec<&Edge> = edges.iter().filter(|e| e.other == 2).collect();
        assert_eq!(hits.len(), 1, "no duplicate edge");
        assert_eq!(hits[0].kind, EdgeKind::Child, "the stronger kind wins");
    }

    #[test]
    fn neighborhood_caps_a_pathological_fan_out() {
        let mut corpus = vec![node(0, 0, None, 1.0)];
        for id in 1..200u32 {
            corpus.push(node(id, 0, Some(0), 1.0));
        }
        assert_eq!(neighborhood(0, &corpus).len(), MAX_EDGES, "the hairball is capped");
    }

    #[test]
    fn neighborhood_of_an_unknown_index_is_empty_not_a_panic() {
        assert!(neighborhood(99, &[node(0, 0, None, 1.0)]).is_empty());
    }

    #[test]
    fn drift_cycles_do_not_break_the_walk() {
        // 0 ↔ 1 mutual drift: each sees the other exactly once, no recursion.
        let mut corpus = vec![node(0, 0, None, 1.0), node(1, 1, None, 1.0)];
        corpus[0].drift.push(1);
        corpus[1].drift.push(0);
        assert_eq!(neighborhood(0, &corpus), vec![Edge { other: 1, kind: EdgeKind::Drift }]);
        assert_eq!(neighborhood(1, &corpus), vec![Edge { other: 0, kind: EdgeKind::Drift }]);
    }

    #[test]
    fn every_context_in_a_real_corpus_has_a_bounded_neighborhood() {
        let corpus = synth_corpus(300, 1, NOW);
        for c in &corpus {
            let edges = neighborhood(c.id, &corpus);
            assert!(edges.len() <= MAX_EDGES, "context {} revealed {} edges", c.id, edges.len());
            for e in &edges {
                assert_ne!(e.other, c.id);
            }
        }
    }
}
