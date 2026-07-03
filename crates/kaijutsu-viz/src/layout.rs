//! Band classification for the time-well context browser.
//!
//! # Architecture
//!
//! **[`assign_band`]** classifies each context into one of three lifecycle
//! [`Band`]s based solely on its `concluded_at` timestamp and an N-most-recent
//! window size. Deterministic and dependency-free.
//!
//! This module previously also carried `CompactingBandLayout`, a pure polar
//! layout engine built on `RadialBands` — deleted 2026-07-03 (no consumer since
//! the 7.8 spiral superseded the ring layout; see `docs/timewell.md`, "Where we
//! are" / "Dead code"). `Band` and `assign_band` remain live; `RadialBands`
//! (`crate::scales`) stays for now as a Stage-1 terracing candidate even though
//! nothing in this crate calls it today.
//!
//! # Fail-loud stance
//!
//! - Invalid config (n_recent_concluded = 0 is allowed as an edge case — all
//!   concluded contexts fall into `Haystack`) is handled explicitly, not clamped.
//! - No silent clamping, no silent fallbacks.

// ─── Band ────────────────────────────────────────────────────────────────────

/// Lifecycle band — the three radial annuli of the time well.
///
/// Band 0 (`Hot`) is the outermost rim (open, active work).
/// Band 1 (`RecentConcluded`) is the mid ring (last N concluded).
/// Band 2 (`Haystack`) is the innermost core (older concluded).
///
/// The numerical band index for use with [`crate::scales::RadialBands`] is
/// available via [`Band::index`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Band {
    /// Open / not concluded.  Outermost ring (index 2).
    Hot,
    /// Concluded, among the N most-recent by `concluded_at`.  Middle ring (index 1).
    RecentConcluded,
    /// Concluded, older than the top-N window.  Innermost core (index 0).
    Haystack,
}

impl Band {
    /// Numeric band index for use with [`crate::scales::RadialBands`].
    ///
    /// | Band              | Index | Ring        |
    /// |-------------------|-------|-------------|
    /// | `Hot`             | 2     | outermost   |
    /// | `RecentConcluded` | 1     | middle      |
    /// | `Haystack`        | 0     | innermost   |
    pub fn index(self) -> usize {
        match self {
            Band::Hot => 2,
            Band::RecentConcluded => 1,
            Band::Haystack => 0,
        }
    }
}

// ─── assign_band ─────────────────────────────────────────────────────────────

/// Context descriptor used by [`assign_band`].
///
/// Ids only need to be `Clone + Ord + Debug` so they can appear in diagnostics and
/// be used as stable sort tie-breakers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextLifecycle<Id> {
    /// Stable context identifier.
    pub id: Id,
    /// Unix-millisecond creation time (informational; not used for band assignment).
    pub created_at: i64,
    /// Unix-millisecond conclusion time, or `None` if the context is still open.
    pub concluded_at: Option<i64>,
}

/// Assign a [`Band`] to each context in `contexts`.
///
/// Rules:
/// - `concluded_at == None` → [`Band::Hot`].
/// - `concluded_at == Some(_)` and among the **`n_recent_concluded` most-recent**
///   by `concluded_at` (ties broken by `id` ascending for determinism) →
///   [`Band::RecentConcluded`].
/// - `concluded_at == Some(_)` and outside the top-N window →
///   [`Band::Haystack`].
///
/// When `n_recent_concluded == 0` all concluded contexts fall into
/// [`Band::Haystack`].
///
/// # Output order
///
/// The output `Vec` is in the same order as `contexts`.
///
/// # Determinism
///
/// Ties on `concluded_at` are broken by `id` ascending, so the assignment is
/// fully deterministic regardless of input ordering.
pub fn assign_band<Id>(
    contexts: &[ContextLifecycle<Id>],
    n_recent_concluded: usize,
) -> Vec<Band>
where
    Id: Clone + Ord,
{
    // Build an ordered list of (concluded_at, &id) for the concluded contexts.
    // We sort descending by concluded_at (most recent first), ties broken by id ascending.
    let mut concluded: Vec<(i64, &Id)> = contexts
        .iter()
        .filter_map(|c| c.concluded_at.map(|ts| (ts, &c.id)))
        .collect();

    // Sort: primary = timestamp descending (most recent first),
    //       secondary = id ascending (stable tie-break).
    concluded.sort_by(|a, b| {
        b.0.cmp(&a.0) // descending timestamp
            .then_with(|| a.1.cmp(b.1)) // ascending id for tie-breaking
    });

    // Collect the ids of the top-N most-recent concluded contexts into a set.
    // Using a BTreeSet for O(log N) lookup and determinism.
    let recent_set: std::collections::BTreeSet<&Id> =
        concluded.iter().take(n_recent_concluded).map(|(_, id)| *id).collect();

    // Map each context to its band.
    contexts
        .iter()
        .map(|c| match c.concluded_at {
            None => Band::Hot,
            Some(_) => {
                if recent_set.contains(&c.id) {
                    Band::RecentConcluded
                } else {
                    Band::Haystack
                }
            }
        })
        .collect()
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    // ── Band::index ────────────────────────────────────────────────────────

    #[test]
    fn band_index_values() {
        assert_eq!(Band::Hot.index(), 2, "Hot is outermost (index 2)");
        assert_eq!(Band::RecentConcluded.index(), 1, "RecentConcluded is middle (index 1)");
        assert_eq!(Band::Haystack.index(), 0, "Haystack is innermost (index 0)");
    }

    // ── assign_band: basic lifecycle rules ─────────────────────────────────

    #[test]
    fn not_concluded_is_hot() {
        let ctx = vec![ContextLifecycle { id: 1u32, created_at: 0, concluded_at: None }];
        let bands = assign_band(&ctx, 10);
        assert_eq!(bands[0], Band::Hot, "open context must be Hot");
    }

    #[test]
    fn concluded_within_top_n_is_recent_concluded() {
        // One concluded context, N=1 → it is the top-1, so RecentConcluded.
        let ctx = vec![ContextLifecycle { id: 1u32, created_at: 0, concluded_at: Some(1000) }];
        let bands = assign_band(&ctx, 1);
        assert_eq!(bands[0], Band::RecentConcluded);
    }

    #[test]
    fn concluded_outside_top_n_is_haystack() {
        // N=0: all concluded → Haystack.
        let ctx = vec![ContextLifecycle { id: 1u32, created_at: 0, concluded_at: Some(1000) }];
        let bands = assign_band(&ctx, 0);
        assert_eq!(bands[0], Band::Haystack, "N=0: all concluded must be Haystack");
    }

    #[test]
    fn n_zero_all_concluded_are_haystack() {
        let ctx: Vec<ContextLifecycle<u32>> = (0..5)
            .map(|i| ContextLifecycle { id: i, created_at: 0, concluded_at: Some(i as i64 * 100) })
            .collect();
        let bands = assign_band(&ctx, 0);
        assert!(bands.iter().all(|b| *b == Band::Haystack), "N=0: every concluded must be Haystack");
    }

    // ── assign_band: 10/11 boundary ────────────────────────────────────────

    #[test]
    fn tenth_most_recent_is_recent_concluded_eleventh_is_haystack() {
        // 12 concluded contexts with distinct timestamps (100, 200, …, 1200).
        // N=10 → top-10 are ids 3..12 (timestamps 300..1200); id 1 (ts=100) and id 2 (ts=200) are Haystack.
        let ctx: Vec<ContextLifecycle<u32>> = (1u32..=12)
            .map(|i| ContextLifecycle {
                id: i,
                created_at: 0,
                concluded_at: Some(i as i64 * 100),
            })
            .collect();
        let bands = assign_band(&ctx, 10);

        // Ids 3..=12 (the 10 most-recent) must be RecentConcluded.
        for (i, entry) in ctx.iter().enumerate() {
            if entry.id >= 3 {
                assert_eq!(
                    bands[i],
                    Band::RecentConcluded,
                    "id {} (ts {}) should be RecentConcluded",
                    entry.id,
                    entry.concluded_at.unwrap()
                );
            } else {
                assert_eq!(
                    bands[i],
                    Band::Haystack,
                    "id {} (ts {}) should be Haystack",
                    entry.id,
                    entry.concluded_at.unwrap()
                );
            }
        }

        // Specifically test the 10th vs 11th most recent (ids 3 vs 2, 0-indexed in sorted desc order).
        // In ctx the contexts are ordered id 1..=12, so:
        // Most recent = id 12 (ts=1200), …, 10th most recent = id 3 (ts=300), 11th = id 2 (ts=200).
        let id3_idx = ctx.iter().position(|c| c.id == 3).unwrap();
        let id2_idx = ctx.iter().position(|c| c.id == 2).unwrap();
        assert_eq!(bands[id3_idx], Band::RecentConcluded, "10th most-recent (id=3) must be RecentConcluded");
        assert_eq!(bands[id2_idx], Band::Haystack, "11th most-recent (id=2) must be Haystack");
    }

    // ── assign_band: tie-breaking ──────────────────────────────────────────

    #[test]
    fn tie_breaking_by_id_ascending_is_stable() {
        // 3 contexts all with the same concluded_at; N=2.
        // Tie-breaking: lower id wins → ids 1 and 2 are RecentConcluded, id 3 is Haystack.
        let ctx = vec![
            ContextLifecycle { id: 3u32, created_at: 0, concluded_at: Some(500) },
            ContextLifecycle { id: 1u32, created_at: 0, concluded_at: Some(500) },
            ContextLifecycle { id: 2u32, created_at: 0, concluded_at: Some(500) },
        ];
        let bands = assign_band(&ctx, 2);

        // Original order: id=3, id=1, id=2
        // id=3 → Haystack (3rd by ascending id at same ts), id=1 → RecentConcluded, id=2 → RecentConcluded.
        let id_to_band: BTreeMap<u32, Band> = ctx.iter().zip(bands.iter()).map(|(c, &b)| (c.id, b)).collect();
        assert_eq!(id_to_band[&1], Band::RecentConcluded, "id=1 should win tie-break (lowest id)");
        assert_eq!(id_to_band[&2], Band::RecentConcluded, "id=2 should be second in tie-break");
        assert_eq!(id_to_band[&3], Band::Haystack, "id=3 should lose tie-break (highest id)");
    }

    #[test]
    fn tie_breaking_is_deterministic_across_input_orders() {
        // Same 3 contexts but shuffled input → same assignment.
        let base = vec![
            ContextLifecycle { id: 1u32, created_at: 0, concluded_at: Some(500) },
            ContextLifecycle { id: 2u32, created_at: 0, concluded_at: Some(500) },
            ContextLifecycle { id: 3u32, created_at: 0, concluded_at: Some(500) },
        ];
        let shuffled = vec![
            ContextLifecycle { id: 3u32, created_at: 0, concluded_at: Some(500) },
            ContextLifecycle { id: 1u32, created_at: 0, concluded_at: Some(500) },
            ContextLifecycle { id: 2u32, created_at: 0, concluded_at: Some(500) },
        ];
        let bands_base = assign_band(&base, 2);
        let bands_shuffled = assign_band(&shuffled, 2);

        let to_map = |ctx: &Vec<ContextLifecycle<u32>>, bands: &Vec<Band>| -> BTreeMap<u32, Band> {
            ctx.iter().zip(bands.iter()).map(|(c, &b)| (c.id, b)).collect()
        };
        assert_eq!(to_map(&base, &bands_base), to_map(&shuffled, &bands_shuffled),
            "band assignment must be independent of input order");
    }

    #[test]
    fn mixed_hot_and_concluded() {
        // 3 open + 3 concluded; N=2.  Top 2 concluded → RecentConcluded.
        let ctx = vec![
            ContextLifecycle { id: 1u32, created_at: 0, concluded_at: None },
            ContextLifecycle { id: 2u32, created_at: 0, concluded_at: None },
            ContextLifecycle { id: 3u32, created_at: 0, concluded_at: None },
            ContextLifecycle { id: 4u32, created_at: 0, concluded_at: Some(100) },
            ContextLifecycle { id: 5u32, created_at: 0, concluded_at: Some(200) },
            ContextLifecycle { id: 6u32, created_at: 0, concluded_at: Some(300) },
        ];
        let bands = assign_band(&ctx, 2);
        let to_map: BTreeMap<u32, Band> = ctx.iter().zip(bands.iter()).map(|(c, &b)| (c.id, b)).collect();

        assert_eq!(to_map[&1], Band::Hot);
        assert_eq!(to_map[&2], Band::Hot);
        assert_eq!(to_map[&3], Band::Hot);
        assert_eq!(to_map[&4], Band::Haystack, "id=4 (oldest concluded) should be Haystack");
        assert_eq!(to_map[&5], Band::RecentConcluded, "id=5 should be RecentConcluded");
        assert_eq!(to_map[&6], Band::RecentConcluded, "id=6 (most recent) should be RecentConcluded");
    }

    // ── Property tests ──────────────────────────────────────────────────

    #[cfg(test)]
    mod props {
        use super::*;
        use proptest::prelude::*;

        // PROP-ASSIGN-BAND-DETERMINISM: assign_band output is independent of input order.
        proptest! {
            #[test]
            fn prop_assign_band_determinism(
                raw in prop::collection::vec(
                    (0u32..50u32, 0i64..1000i64, prop::option::of(0i64..5000i64)),
                    0..15,
                ),
            ) {
                // Deduplicate by id.
                let mut seen: BTreeMap<u32, (i64, Option<i64>)> = BTreeMap::new();
                for (id, created, concluded) in &raw {
                    seen.insert(*id, (*created, *concluded));
                }
                let base: Vec<ContextLifecycle<u32>> = seen.iter()
                    .map(|(id, (c, cc))| ContextLifecycle { id: *id, created_at: *c, concluded_at: *cc })
                    .collect();

                // Shuffle by reversing.
                let mut shuffled = base.clone();
                shuffled.reverse();

                let bands_base    = assign_band(&base, 5);
                let bands_shuffled = assign_band(&shuffled, 5);

                // Build id→band maps and compare.
                let to_map = |ctx: &Vec<ContextLifecycle<u32>>, bands: &Vec<Band>| -> BTreeMap<u32, Band> {
                    ctx.iter().zip(bands.iter()).map(|(c, &b)| (c.id, b)).collect()
                };
                prop_assert_eq!(
                    to_map(&base, &bands_base),
                    to_map(&shuffled, &bands_shuffled),
                    "assign_band must be independent of input order"
                );
            }
        }
    }
}
