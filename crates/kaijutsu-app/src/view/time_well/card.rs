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

use bevy::math::Vec3;
use kaijutsu_client::ContextInfo;
use kaijutsu_types::ContextId;
use kaijutsu_viz::layout::{
    Band, CompactingBandLayout, ContextEntry, ContextLifecycle, LayoutPos, assign_band,
};

/// How many of the most-recent concluded contexts live in band 1
/// (`RecentConcluded`) before falling into the band-2 haystack. Matches the
/// "last N = 10" rule from the design doc.
pub const N_RECENT_CONCLUDED: usize = 10;

/// 3D geometry for lifting a 2D [`LayoutPos`] into the well.
///
/// The 2D layout places each card on a ring (`x`, `y`) whose radius already
/// encodes the band; the lift adds the **depth** axis so the rings stack into a
/// funnel. Hot (the outermost, largest ring) sits at the rim near the camera;
/// each colder band recedes by `depth_step` into the well.
#[derive(Debug, Clone, Copy)]
pub struct WellGeometry {
    /// Per-band depth (along -Z, into the screen), indexed by [`Band::index`]:
    /// `[Haystack, RecentConcluded, Hot]`.
    pub band_depth: [f32; 3],
}

impl WellGeometry {
    /// A funnel where each colder band recedes by `depth_step` units.
    ///
    /// Hot (rim) is at depth 0; recent-concluded one step back; the haystack two
    /// steps back. The 2D layout's smaller radii for colder bands plus this
    /// receding depth give the well its funnel profile.
    pub fn stepped(depth_step: f32) -> Self {
        Self {
            band_depth: [
                -2.0 * depth_step, // Haystack  (index 0, innermost, deepest)
                -depth_step,       // RecentConcluded (index 1)
                0.0,               // Hot (index 2, rim, at the mouth of the well)
            ],
        }
    }
}

impl Default for WellGeometry {
    fn default() -> Self {
        Self::stepped(200.0)
    }
}

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
/// - **Hot** — id ascending (UUIDv7 = creation order, unique); this is what the
///   `0–9` digit keys address.
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

    // Hot: id ascending (creation order; matches the `0–9` addressing).
    let mut hot: Vec<ContextId> = in_band(Band::Hot).iter().map(|c| c.id).collect();
    hot.sort_unstable();

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

/// Run the contexts through the compacting band layout, returning a position per
/// context id. Each entry's `order_key` is its index within its band's
/// [`BandOrders`] vector — so the angular slot, the keyboard slot, and the digit
/// addressing are all the same ranking, by construction.
pub fn layout_positions(
    contexts: &[ContextInfo],
    bands: &[Band],
    orders: &BandOrders,
    layout: &CompactingBandLayout,
) -> std::collections::BTreeMap<ContextId, LayoutPos> {
    debug_assert_eq!(
        contexts.len(),
        bands.len(),
        "bands must align with contexts"
    );

    // id → slot index within its band (ids are unique per band, and each id
    // lives in exactly one band's vector, so one flat map suffices).
    let mut rank: std::collections::HashMap<ContextId, i64> = std::collections::HashMap::new();
    for band_vec in orders {
        for (i, id) in band_vec.iter().enumerate() {
            rank.insert(*id, i as i64);
        }
    }

    let entries: Vec<ContextEntry<ContextId>> = contexts
        .iter()
        .zip(bands.iter())
        .map(|(c, &band)| ContextEntry {
            id: c.id,
            band,
            // A context always appears in its band's order vector; default 0
            // only as a defensive fallback (never hit in practice).
            order_key: rank.get(&c.id).copied().unwrap_or(0),
        })
        .collect();

    layout.compute(&entries)
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

/// Lift a 2D [`LayoutPos`] into the 3D well.
///
/// `(x, y)` are the ring-plane coordinates from the layout; the band selects the
/// depth from [`WellGeometry`]. The app writes the result onto the card's
/// `Transform` (tweened, not snapped).
pub fn lift(pos: &LayoutPos, geom: &WellGeometry) -> Vec3 {
    Vec3::new(pos.x, pos.y, geom.band_depth[pos.band.index()])
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
        }
    }

    /// A deterministic ContextId from a single discriminant byte (UUIDv7-shaped:
    /// the leading bytes order the id, which is what the layout ranks on).
    fn id_of(n: u8) -> ContextId {
        let mut b = [0u8; 16];
        b[0] = n;
        ContextId::from_bytes(b)
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
    fn lift_passes_xy_and_maps_band_to_depth() {
        let geom = WellGeometry::stepped(100.0);
        let hot = lift(
            &LayoutPos {
                band: Band::Hot,
                x: 3.0,
                y: 4.0,
            },
            &geom,
        );
        assert_eq!(hot, Vec3::new(3.0, 4.0, 0.0));

        let recent = lift(
            &LayoutPos {
                band: Band::RecentConcluded,
                x: 1.0,
                y: 2.0,
            },
            &geom,
        );
        assert_eq!(recent, Vec3::new(1.0, 2.0, -100.0));

        let haystack = lift(
            &LayoutPos {
                band: Band::Haystack,
                x: 0.0,
                y: 0.0,
            },
            &geom,
        );
        assert_eq!(haystack, Vec3::new(0.0, 0.0, -200.0));
    }

    #[test]
    fn layout_assigns_a_position_per_context() {
        use kaijutsu_viz::layout::{CompactingBandLayout, LayoutConfig};

        let contexts: Vec<ContextInfo> = (0..3u8).map(|n| ctx(id_of(n), "")).collect();
        let bands = assign_bands(&contexts);
        let layout = CompactingBandLayout::new(LayoutConfig::default_config());
        let orders = band_orders(&contexts, &bands, &std::collections::HashMap::new());
        let positions = layout_positions(&contexts, &bands, &orders, &layout);

        assert_eq!(positions.len(), 3);
        for c in &contexts {
            assert!(
                positions.contains_key(&c.id),
                "missing position for {:?}",
                c.id
            );
        }
    }

    #[test]
    fn band1_angle_tracks_conclusion_recency_not_id_order() {
        use kaijutsu_viz::layout::{CompactingBandLayout, LayoutConfig};
        let layout = CompactingBandLayout::new(LayoutConfig::default_config());

        // Three concluded contexts whose *id* order (1<2<3) deliberately differs
        // from their *conclusion* order: id 2 concluded last, id 1 first. With all
        // three inside the recent-concluded window they share band 1, where angle
        // must encode conclusion recency — so the newest (id 2) takes the anchor
        // slot, not the lowest id.
        let mk = |n: u8, concluded: u64| {
            let mut c = ctx(id_of(n), "");
            c.concluded_at = Some(concluded);
            c
        };
        let contexts = vec![
            mk(1, 100), // oldest conclusion
            mk(2, 300), // newest conclusion
            mk(3, 200), // middle
        ];
        let bands = assign_bands(&contexts);
        assert!(
            bands.iter().all(|&b| b == Band::RecentConcluded),
            "all three should be recent-concluded for this N"
        );
        let orders = band_orders(&contexts, &bands, &std::collections::HashMap::new());
        let pos = layout_positions(&contexts, &bands, &orders, &layout);

        // Band-1 mid radius for the default config (total 300, 3 bands) is 150;
        // slot 0 sits at the band's start angle (0 here) → (150, 0).
        let r = 150.0_f32;
        let slot = |s: usize| {
            let pitch = std::f64::consts::TAU / 12.0; // default_config pitch
            let theta = s as f64 * pitch;
            ((r as f64 * theta.cos()) as f32, (r as f64 * theta.sin()) as f32)
        };
        let approx = |a: (f32, f32), b: (f32, f32)| {
            (a.0 - b.0).abs() < 1e-2 && (a.1 - b.1).abs() < 1e-2
        };

        let p = |n: u8| (pos[&id_of(n)].x, pos[&id_of(n)].y);
        // Newest conclusion (id 2) → slot 0 (anchor); middle (id 3) → slot 1;
        // oldest (id 1) → slot 2. If angle had keyed on id order, id 1 would be
        // at slot 0 and this would fail.
        assert!(approx(p(2), slot(0)), "newest concluded must take the anchor slot 0, got {:?}", p(2));
        assert!(approx(p(3), slot(1)), "middle conclusion must take slot 1, got {:?}", p(3));
        assert!(approx(p(1), slot(2)), "oldest conclusion must take slot 2, got {:?}", p(1));
    }

    #[test]
    fn append_does_not_move_existing_hot_cards() {
        use kaijutsu_viz::layout::{CompactingBandLayout, LayoutConfig};
        let layout = CompactingBandLayout::new(LayoutConfig::default_config());

        let before: Vec<ContextInfo> = (1..=3u8).map(|n| ctx(id_of(n), "")).collect();
        let bands_b = assign_bands(&before);
        let orders_b = band_orders(&before, &bands_b, &std::collections::HashMap::new());
        let pos_b = layout_positions(&before, &bands_b, &orders_b, &layout);

        // Append a newer context (larger id → later in time order → growing edge).
        let mut after = before.clone();
        after.push(ctx(id_of(4), ""));
        let bands_a = assign_bands(&after);
        let orders_a = band_orders(&after, &bands_a, &std::collections::HashMap::new());
        let pos_a = layout_positions(&after, &bands_a, &orders_a, &layout);

        // Every pre-existing card keeps its exact slot.
        for c in &before {
            assert_eq!(pos_b[&c.id], pos_a[&c.id], "append moved {:?}", c.id);
        }
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

        // Hot: two open contexts (ids 5, 3) → expect id-ascending [3, 5].
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
            vec![id_of(3), id_of(5)],
            "hot orders by id ascending"
        );
        assert_eq!(
            orders[Band::RecentConcluded.index()],
            vec![id_of(9), id_of(8)],
            "recent orders newest-conclusion first"
        );
        assert!(orders[Band::Haystack.index()].is_empty());
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
