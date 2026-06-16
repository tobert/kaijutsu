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
}

/// Build a [`CardData`] from a [`ContextInfo`] and its pre-assigned [`Band`].
///
/// Band is passed in (not derived here) because it depends on the *whole set* —
/// see [`assign_bands`]. Everything else is a per-context field map with the
/// fallbacks the design doc's card table specifies.
pub fn card_from(info: &ContextInfo, band: Band) -> CardData {
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

/// Run the contexts through the compacting band layout, returning a position per
/// context id.
///
/// `bands` must be positionally aligned with `contexts` (as produced by
/// [`assign_bands`]). The per-context `order_key` is the context's **rank in
/// time order** — ids are UUIDv7, so sorting by id is creation order and is
/// guaranteed unique, satisfying the layout's "unique `order_key` per band"
/// precondition (no ties → stable slots independent of poll order).
pub fn layout_positions(
    contexts: &[ContextInfo],
    bands: &[Band],
    layout: &CompactingBandLayout,
) -> std::collections::BTreeMap<ContextId, LayoutPos> {
    debug_assert_eq!(
        contexts.len(),
        bands.len(),
        "bands must align with contexts"
    );

    // Rank by id (UUIDv7 = time order, unique) → dense, collision-free order_key.
    let mut ids: Vec<ContextId> = contexts.iter().map(|c| c.id).collect();
    ids.sort_unstable();
    let rank = |id: &ContextId| ids.binary_search(id).expect("id present") as i64;

    let entries: Vec<ContextEntry<ContextId>> = contexts
        .iter()
        .zip(bands.iter())
        .map(|(c, &band)| ContextEntry {
            id: c.id,
            band,
            order_key: rank(&c.id),
        })
        .collect();

    layout.compute(&entries)
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
        let labeled = card_from(&ctx(id, "my work"), Band::Hot);
        assert_eq!(labeled.title, "my work");

        let unlabeled = card_from(&ctx(id, ""), Band::Hot);
        assert_eq!(unlabeled.title, id.short());
    }

    #[test]
    fn accent_is_context_type_then_provider() {
        let mut info = ctx(id_of(1), "x");
        info.context_type = "coder".to_string();
        info.provider = "anthropic".to_string();
        assert_eq!(card_from(&info, Band::Hot).accent, "coder");

        info.context_type = String::new();
        assert_eq!(card_from(&info, Band::Hot).accent, "anthropic");
    }

    #[test]
    fn model_badge_joins_provider_and_model() {
        let mut info = ctx(id_of(1), "x");
        info.provider = "anthropic".to_string();
        info.model = "claude-opus-4-8".to_string();
        assert_eq!(
            card_from(&info, Band::Hot).model_badge,
            "anthropic/claude-opus-4-8"
        );

        info.model = String::new();
        assert_eq!(card_from(&info, Band::Hot).model_badge, "anthropic");

        info.provider = String::new();
        assert_eq!(card_from(&info, Band::Hot).model_badge, "");
    }

    #[test]
    fn fork_badge_present_only_for_nonempty_fork_kind() {
        let mut info = ctx(id_of(1), "x");
        assert_eq!(card_from(&info, Band::Hot).fork_badge, None);

        info.fork_kind = Some(String::new());
        assert_eq!(card_from(&info, Band::Hot).fork_badge, None);

        info.fork_kind = Some("subtree".to_string());
        assert_eq!(
            card_from(&info, Band::Hot).fork_badge,
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
        let positions = layout_positions(&contexts, &bands, &layout);

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
    fn append_does_not_move_existing_hot_cards() {
        use kaijutsu_viz::layout::{CompactingBandLayout, LayoutConfig};
        let layout = CompactingBandLayout::new(LayoutConfig::default_config());

        let before: Vec<ContextInfo> = (1..=3u8).map(|n| ctx(id_of(n), "")).collect();
        let bands_b = assign_bands(&before);
        let pos_b = layout_positions(&before, &bands_b, &layout);

        // Append a newer context (larger id → later in time order → growing edge).
        let mut after = before.clone();
        after.push(ctx(id_of(4), ""));
        let bands_a = assign_bands(&after);
        let pos_a = layout_positions(&after, &bands_a, &layout);

        // Every pre-existing card keeps its exact slot.
        for c in &before {
            assert_eq!(pos_b[&c.id], pos_a[&c.id], "append moved {:?}", c.id);
        }
    }
}
