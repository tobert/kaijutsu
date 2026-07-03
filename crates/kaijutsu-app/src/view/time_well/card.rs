//! Pure `ContextInfo` → card-model mapping, band assignment, layout, and the
//! `LayoutPos → Vec3` well-lift.
//!
//! This is the seam between the zero-dependency `kaijutsu-viz` substrate (scales,
//! join, layout — all pure 2D / no glam) and the Bevy app. Per `docs/viz-substrate.md`,
//! the lift to `glam::Vec3` lives **here**, on the app side, so the substrate stays
//! free of a `glam` version lockstep.
//!
//! Everything in this module is pure (no Bevy `World`, no GPU) and unit-tested.
//! The Bevy systems in the sibling modules call these functions; the cards they
//! produce are written onto entity components by the join system.

use bevy::math::{Quat, Vec3};
use kaijutsu_client::ContextInfo;
use kaijutsu_types::ContextId;
use kaijutsu_viz::layout::{Band, ContextLifecycle, assign_band};

/// How many of the most-recent concluded contexts live in band 1
/// (`RecentConcluded`) before falling into the band-2 haystack. Matches the
/// "last N = 10" rule from the design doc.
pub const N_RECENT_CONCLUDED: usize = 10;


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
    /// Kernel-synthesized semantic-cluster label, set only for haystack (band-2)
    /// cards that belong to a cluster. `None` for hot/recent cards and for
    /// unclustered haystack cards.
    pub cluster_label: Option<String>,
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
/// see [`assign_bands`]. Everything else is a per-context field map with the
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
        // Only haystack cards carry a cluster label; the caller passes `None`
        // for hot/recent cards (their angle encodes a different axis).
        cluster_label: if band == Band::Haystack {
            cluster_label
        } else {
            None
        },
    }
}

/// Assign a [`Band`] to each context, aligned positionally with `contexts`.
///
/// Open (no `concluded_at`) → hot; concluded → recent-concluded (top-N by
/// `concluded_at`) or haystack. Archived contexts are expected to be filtered
/// out *before* this call (the well doesn't show them) — see
/// `sync::sync_time_well`.
pub fn assign_bands(contexts: &[ContextInfo]) -> Vec<Band> {
    let lifecycles: Vec<ContextLifecycle<ContextId>> = contexts
        .iter()
        .map(|c| ContextLifecycle {
            id: c.id,
            created_at: c.created_at as i64,
            concluded_at: c.concluded_at.map(|ts| ts as i64),
        })
        .collect();

    assign_band(&lifecycles, N_RECENT_CONCLUDED)
}

/// Each band's context ids in **angular slot order**, indexed by [`Band::index`]
/// (`[Haystack, RecentConcluded, Hot]`). This is the single source of slot
/// order: the layout derives every `order_key` from it (so angle == slot), and
/// keyboard navigation walks the same vectors (so the keys match the visuals).
///
/// Per-band ordering (the orthogonal-meaning rule from "The three bands"):
/// - **Hot** — id descending (UUIDv7 = creation order, unique; newest first, so
///   the mouth is the newest open context); this is what the `0–9` digit keys
///   temporarily address (Stage 0 tourniquet — see `docs/timewell.md`).
/// - **RecentConcluded** — most-recently-concluded first (`concluded_at`
///   descending, id-tiebroken), so band-1 angle is "a clock of what I just
///   finished" and slot 0 is the newest conclusion.
/// - **Haystack** — semantic-cluster grouping (`haystack_order_keys`):
///   same-cluster contexts adjacent, unclustered trailing.
pub type BandOrders = [Vec<ContextId>; 3];

/// Compute each band's [`BandOrders`] slot order over the current set.
pub fn band_orders(
    contexts: &[ContextInfo],
    bands: &[Band],
    cluster_of: &std::collections::HashMap<ContextId, ClusterAssignment>,
) -> BandOrders {
    debug_assert_eq!(
        contexts.len(),
        bands.len(),
        "bands must align with contexts"
    );
    let in_band = |want: Band| -> Vec<&ContextInfo> {
        contexts
            .iter()
            .zip(bands.iter())
            .filter(move |(_, b)| **b == want)
            .map(|(c, _)| c)
            .collect()
    };

    // Hot: id descending (newest first — mouth = newest open context; matches
    // the `0–9` addressing).
    let mut hot: Vec<ContextId> = in_band(Band::Hot).iter().map(|c| c.id).collect();
    hot.sort_unstable_by(|a, b| b.cmp(a));

    // RecentConcluded: newest conclusion first, id-tiebroken.
    let mut recent = in_band(Band::RecentConcluded);
    recent.sort_by(|a, b| b.concluded_at.cmp(&a.concluded_at).then(a.id.cmp(&b.id)));
    let recent: Vec<ContextId> = recent.into_iter().map(|c| c.id).collect();

    // Haystack: cluster-grouped (sort by the haystack rank).
    let haystack_ids: Vec<ContextId> = in_band(Band::Haystack).iter().map(|c| c.id).collect();
    let keys = haystack_order_keys(&haystack_ids, cluster_of);
    let mut haystack = haystack_ids;
    haystack.sort_by_key(|id| keys[id]);

    // Index order is [Haystack, RecentConcluded, Hot] = Band::index.
    [haystack, recent, hot]
}

/// Dense, collision-free order keys for the haystack band that group same-cluster
/// contexts angularly adjacent.
///
/// Contexts are ranked `0..n` after sorting by `(cluster_id, id)`, with
/// **unclustered** contexts (no entry in `cluster_of`) trailing after all
/// clusters (sorted among themselves by id). This makes band-2 angle encode
/// *semantic cluster* — the design's job for the haystack — while staying
/// deterministic: the keys depend only on cluster membership and id, never on
/// input order, so re-deriving each poll is stable until clustering changes.
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
/// its ancestry lights up (`docs/viz-substrate.md`, band-0 lineage overlay).
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
// VORTEX SPIRAL (the "one continuous well" layout)
// ============================================================================
//
// Instead of three discrete rings, every card sits on a single spiral funnel
// indexed mouth → throat. Radius shrinks and depth grows geometrically per
// index, so cards wind inward and downward and *asymptotically crowd the event
// horizon* — the older a context (the further along the spiral), the deeper it
// falls toward the singularity. Append-stable: a card's position depends only on
// its integer index, so appending never reflows earlier cards.

/// Rim radius at the mouth (index 0). Pulled in so cards sit closer together
/// rather than flung wide across the rim.
const SPIRAL_R_MOUTH: f32 = 330.0;
/// Radius the spiral asymptotes to — the event-horizon ring at the throat. Cards
/// only approach it as the spiral grows long; with few contexts the arm stays in
/// the upper funnel, leaving an *empty* stretch above the horizon (nothing has
/// fallen in yet — it fills as contexts age). That gap is meaningful, not a bug.
const SPIRAL_R_THROAT: f32 = 48.0;
/// Throat depth (funnel-local −Z) the spiral asymptotes to.
const SPIRAL_DEPTH: f32 = -560.0;
/// Per-index geometric decay (how fast the spiral winds in + down). Gentle, so a
/// short arm spreads evenly down the upper funnel instead of bunching at center.
const SPIRAL_DECAY: f32 = 0.93;
/// Radians wound per index (~12 cards per revolution) — a tighter, denser arm.
const SPIRAL_ANGLE_STEP: f32 = 0.50;
/// Card base scale at the throat (mouth cards are 1.0). Kept high so cards stay
/// readable as they descend (bigger overall, per Amy).
const SPIRAL_SCALE_THROAT: f32 = 0.52;

/// Funnel-local spiral position for the card at `index` (0 = mouth). See module
/// note above: geometric inward/downward decay so cards pile toward the throat.
fn spiral_local(index: usize) -> Vec3 {
    let f = SPIRAL_DECAY.powi(index as i32); // 1 → 0 as index grows
    let radius = SPIRAL_R_THROAT + (SPIRAL_R_MOUTH - SPIRAL_R_THROAT) * f;
    let depth = SPIRAL_DEPTH * (1.0 - f);
    let angle = index as f32 * SPIRAL_ANGLE_STEP;
    Vec3::new(radius * angle.cos(), radius * angle.sin(), depth)
}

/// World position of the card at spiral `index`: the funnel-local spiral tipped
/// back by [`WELL_TILT`] (same recline as everything else in the well).
pub fn spiral_pos(index: usize) -> Vec3 {
    well_tilt_quat() * spiral_local(index)
}

/// Per-card base scale along the spiral: 1.0 at the mouth, shrinking toward
/// [`SPIRAL_SCALE_THROAT`] at the throat so the vortex reads as receding depth.
pub fn spiral_scale(index: usize) -> f32 {
    let f = SPIRAL_DECAY.powi(index as i32);
    SPIRAL_SCALE_THROAT + (1.0 - SPIRAL_SCALE_THROAT) * f
}

/// The whole well as one ordered spiral, **mouth → throat**: live (hot) first,
/// then recently-concluded, then the haystack — each zone kept in its existing
/// slot order (so recency + cluster grouping survive inside the sequence). The
/// index into this vector is both a card's position on the vortex and its
/// odometer address (Left/Right = ±1, Up/Down = ±10, digits = the first decade).
pub fn spiral_order(
    contexts: &[ContextInfo],
    bands: &[Band],
    cluster_of: &std::collections::HashMap<ContextId, ClusterAssignment>,
) -> Vec<ContextId> {
    let orders = band_orders(contexts, bands, cluster_of);
    let mut out = Vec::new();
    for b in [Band::Hot, Band::RecentConcluded, Band::Haystack] {
        out.extend_from_slice(&orders[b.index()]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let labeled = card_from(&ctx(id, "my work"), Band::Hot, None);
        assert_eq!(labeled.title, "my work");

        let unlabeled = card_from(&ctx(id, ""), Band::Hot, None);
        assert_eq!(unlabeled.title, id.short());
    }

    #[test]
    fn accent_is_context_type_then_provider() {
        let mut info = ctx(id_of(1), "x");
        info.context_type = "coder".to_string();
        info.provider = "anthropic".to_string();
        assert_eq!(card_from(&info, Band::Hot, None).accent, "coder");

        info.context_type = String::new();
        assert_eq!(card_from(&info, Band::Hot, None).accent, "anthropic");
    }

    #[test]
    fn model_badge_joins_provider_and_model() {
        let mut info = ctx(id_of(1), "x");
        info.provider = "anthropic".to_string();
        info.model = "claude-opus-4-8".to_string();
        assert_eq!(
            card_from(&info, Band::Hot, None).model_badge,
            "anthropic/claude-opus-4-8"
        );

        info.model = String::new();
        assert_eq!(card_from(&info, Band::Hot, None).model_badge, "anthropic");

        info.provider = String::new();
        assert_eq!(card_from(&info, Band::Hot, None).model_badge, "");
    }

    #[test]
    fn fork_badge_present_only_for_nonempty_fork_kind() {
        let mut info = ctx(id_of(1), "x");
        assert_eq!(card_from(&info, Band::Hot, None).fork_badge, None);

        info.fork_kind = Some(String::new());
        assert_eq!(card_from(&info, Band::Hot, None).fork_badge, None);

        info.fork_kind = Some("subtree".to_string());
        assert_eq!(
            card_from(&info, Band::Hot, None).fork_badge,
            Some("subtree".to_string())
        );
    }

    #[test]
    fn open_contexts_are_hot_concluded_split_recent_and_haystack() {
        // 12 concluded contexts with ascending concluded_at, plus one open.
        let mut contexts: Vec<ContextInfo> = (0..12u8)
            .map(|n| {
                let mut c = ctx(id_of(n), "");
                c.concluded_at = Some(100 + n as u64); // older → smaller
                c
            })
            .collect();
        let open = {
            let mut c = ctx(id_of(200), "");
            c.created_at = 999;
            c
        };
        contexts.push(open);

        let bands = assign_bands(&contexts);

        // The open one is hot.
        assert_eq!(bands[12], Band::Hot);

        // Of the 12 archived, the 10 most-recent (created_at 2..=11) are
        // RecentConcluded; the 2 oldest (0,1) fall to the haystack.
        let recent = bands[..12]
            .iter()
            .filter(|&&b| b == Band::RecentConcluded)
            .count();
        let haystack = bands[..12].iter().filter(|&&b| b == Band::Haystack).count();
        assert_eq!(recent, N_RECENT_CONCLUDED);
        assert_eq!(haystack, 2);
        // The two oldest specifically.
        assert_eq!(bands[0], Band::Haystack);
        assert_eq!(bands[1], Band::Haystack);
    }

    #[test]
    fn spiral_winds_inward_and_down_then_asymptotes() {
        let r = |v: Vec3| (v.x * v.x + v.y * v.y).sqrt();
        let (mouth, mid, deep) = (spiral_local(0), spiral_local(10), spiral_local(40));
        assert!(r(mouth) > r(mid) && r(mid) > r(deep), "radius winds inward");
        assert!(mid.z < mouth.z && deep.z < mid.z, "descends toward the throat");
        // Cards pile at the event horizon rather than collapsing through it.
        assert!(r(deep) >= SPIRAL_R_THROAT - 1.0, "radius asymptotes to the horizon");
        assert!(deep.z > SPIRAL_DEPTH - 1.0, "depth asymptotes to the throat");
    }

    #[test]
    fn spiral_pos_is_append_stable_and_tipped() {
        assert_eq!(spiral_pos(7), spiral_pos(7), "position keys only on index");
        // After the recline the funnel's depth maps mostly to world-Y, so the
        // robust world-space invariant is "deeper cards sit lower". (Funnel-local
        // depth monotonicity is covered by `spiral_winds_inward_and_down…`.)
        let (near, far) = (spiral_pos(2), spiral_pos(30));
        assert!(far.y < near.y, "deeper card sits lower after the recline");
    }

    #[test]
    fn spiral_scale_shrinks_to_a_floor() {
        assert!(spiral_scale(0) > spiral_scale(8), "cards shrink down the spiral");
        assert!(spiral_scale(0) <= 1.0 + 1e-4, "mouth scale ~1.0");
        assert!(
            spiral_scale(500) >= SPIRAL_SCALE_THROAT - 1e-4,
            "scale is floored at the throat"
        );
    }

    #[test]
    fn spiral_order_runs_hot_then_recent_then_haystack() {
        let contexts = vec![
            ctx(id_of(3), "hay"),
            ctx(id_of(1), "hot"),
            ctx(id_of(2), "rec"),
        ];
        // Bands aligned with contexts (not derived here — we're testing the flatten).
        let bands = vec![Band::Haystack, Band::Hot, Band::RecentConcluded];
        let order = spiral_order(&contexts, &bands, &std::collections::HashMap::new());
        assert_eq!(order.first(), Some(&id_of(1)), "hot leads at the mouth");
        assert_eq!(order.last(), Some(&id_of(3)), "haystack trails at the throat");
        assert_eq!(order, vec![id_of(1), id_of(2), id_of(3)], "hot → recent → haystack");
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

    #[test]
    fn band_orders_rank_each_band_by_its_own_axis() {
        use kaijutsu_viz::layout::Band;
        use std::collections::HashMap;

        // Hot: two open contexts (ids 5, 3) → expect id-descending [5, 3]
        // (newest first, Stage 0 tourniquet — mouth = newest open context).
        let mut hot_a = ctx(id_of(5), "");
        hot_a.created_at = 10;
        let mut hot_b = ctx(id_of(3), "");
        hot_b.created_at = 11;
        // Recent: two concluded; id 9 concluded later than id 8 → recency [9, 8].
        let mut rec_old = ctx(id_of(8), "");
        rec_old.concluded_at = Some(100);
        let mut rec_new = ctx(id_of(9), "");
        rec_new.concluded_at = Some(200);

        let contexts = vec![hot_a, hot_b, rec_old, rec_new];
        // Force the bands explicitly (don't depend on assign_bands' N window here).
        let bands = vec![Band::Hot, Band::Hot, Band::RecentConcluded, Band::RecentConcluded];

        let orders = band_orders(&contexts, &bands, &HashMap::new());
        assert_eq!(
            orders[Band::Hot.index()],
            vec![id_of(5), id_of(3)],
            "hot orders by id descending (newest first)"
        );
        assert_eq!(
            orders[Band::RecentConcluded.index()],
            vec![id_of(9), id_of(8)],
            "recent orders newest-conclusion first"
        );
        assert!(orders[Band::Haystack.index()].is_empty());
    }

    #[test]
    fn hot_band_puts_newest_at_the_mouth() {
        use kaijutsu_viz::layout::Band;
        use std::collections::HashMap;

        // Several open (hot) contexts with increasing ids (creation order).
        let contexts: Vec<ContextInfo> = (1..=5u8).map(|n| ctx(id_of(n), "")).collect();
        let bands = vec![Band::Hot; contexts.len()];

        let order = spiral_order(&contexts, &bands, &HashMap::new());

        // The mouth (index 0) must be the newest context (highest id), not the
        // oldest — Stage 0 tourniquet: the well is a terminal multiplexer, not
        // a creation-order log.
        assert_eq!(
            order.first(),
            Some(&id_of(5)),
            "mouth (index 0) must be the newest open context"
        );
        assert_eq!(
            order,
            vec![id_of(5), id_of(4), id_of(3), id_of(2), id_of(1)],
            "hot band runs id-descending, newest to oldest"
        );
    }

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
    fn cluster_label_set_only_for_haystack() {
        let info = ctx(id_of(1), "x");
        // Haystack card carries the label.
        let hay = card_from(&info, Band::Haystack, Some("rust".to_string()));
        assert_eq!(hay.cluster_label.as_deref(), Some("rust"));
        // Hot / recent cards never carry a cluster label, even if one is passed.
        let hot = card_from(&info, Band::Hot, Some("rust".to_string()));
        assert_eq!(hot.cluster_label, None);
        let recent = card_from(&info, Band::RecentConcluded, Some("rust".to_string()));
        assert_eq!(recent.cluster_label, None);
    }
}
