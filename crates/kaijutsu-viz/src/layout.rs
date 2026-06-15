//! Compacting-band layout for the time-well context browser.
//!
//! # Architecture
//!
//! Two pure pieces compose the layout:
//!
//! 1. **[`assign_band`]** — classifies each context into one of three lifecycle
//!    [`Band`]s based solely on its `concluded_at` timestamp and an N-most-recent
//!    window size.  Deterministic and dependency-free.
//!
//! 2. **[`CompactingBandLayout`]** — given a set of `(id, band, order_key)` triples,
//!    produces a `BTreeMap<id, LayoutPos>` that places every context in polar
//!    coordinates, converted to Cartesian `(x, y)`.  Radius derives from
//!    [`crate::scales::RadialBands`]; angle is a fixed-pitch slot within the band,
//!    ordered by `order_key` ascending.
//!
//! # Stateless finding
//!
//! `CompactingBandLayout::compute` is a **pure function** — it carries no mutable
//! state between calls.  Both motion invariants (append and compact) hold naturally:
//!
//! - **INV-append:** A new context with the maximum `order_key` in its band gets
//!   the next slot at the growing edge.  All lower-`order_key` contexts keep their
//!   slot indices unchanged, so their `LayoutPos` values are byte-for-byte identical.
//!
//! - **INV-compact:** Removing a context reassigns slot indices only for contexts
//!   with a higher `order_key` in the same band (each shifts left by one pitch).
//!   Contexts in other bands are untouched.
//!
//! A `LayoutState` struct is therefore **unnecessary** for the compacting model.
//! The stable `order_key` plays the role of the persisted slot table that egui_graphs
//! warns about — because `order_key` is already durable (a CRDT tick / UUIDv7 from
//! the kernel), the slot assignment function `slot_index = rank(order_key_in_band)`
//! is re-derivable from the current input set alone, and the two invariants are
//! automatically satisfied without storing anything between calls.
//!
//! **Precondition:** positions are a pure function of the input set *provided
//! `order_key` is unique within each band*, which the kernel guarantees (CRDT
//! ticks / UUIDv7 are never reused).  When order_keys tie within a band, slot
//! assignment depends on input order (because `sort_by_key` is stable), so
//! callers must not rely on positional stability across input-order changes in
//! that case.
//!
//! # Fail-loud stance
//!
//! - Duplicate ids in the input → `panic!` (naming the id).
//! - Invalid config (pitch ≤ 0, total_radius ≤ 0, n_recent_concluded = 0 is
//!   allowed as an edge case — all concluded contexts fall into `Haystack`) →
//!   `assert!`.
//! - No silent clamping, no silent fallbacks.

use std::collections::BTreeMap;

use crate::scales::RadialBands;

// ─── Band ────────────────────────────────────────────────────────────────────

/// Lifecycle band — the three radial annuli of the time well.
///
/// Band 0 (`Hot`) is the outermost rim (open, active work).
/// Band 1 (`RecentConcluded`) is the mid ring (last N concluded).
/// Band 2 (`Haystack`) is the innermost core (older concluded).
///
/// The numerical band index for use with [`RadialBands`] is available via
/// [`Band::index`].
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
    /// Numeric band index for use with [`RadialBands`].
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

// ─── LayoutPos ───────────────────────────────────────────────────────────────

/// Output position for one context in the time-well layout.
#[derive(Debug, Clone, PartialEq)]
pub struct LayoutPos {
    /// The lifecycle band this context is placed in.
    pub band: Band,
    /// Cartesian X coordinate (f32; the Bevy app lifts this to Vec3 at its boundary).
    pub x: f32,
    /// Cartesian Y coordinate (f32).
    pub y: f32,
}

// ─── LayoutConfig ─────────────────────────────────────────────────────────────

/// Per-band angular configuration for [`CompactingBandLayout`].
#[derive(Debug, Clone)]
pub struct BandAngleConfig {
    /// Starting angle for this band, in radians.
    pub start_angle: f64,
    /// Angular pitch (slot-to-slot increment), in radians.
    ///
    /// Must be > 0.
    pub pitch: f64,
}

/// Configuration for [`CompactingBandLayout`].
///
/// # Panics
///
/// [`CompactingBandLayout::new`] panics if:
/// - `total_radius` ≤ 0.
/// - Any band's `pitch` ≤ 0.
#[derive(Debug, Clone)]
pub struct LayoutConfig {
    /// Total radius of the well, passed to [`RadialBands`] (3 bands).
    pub total_radius: f64,
    /// Per-band angle config indexed by [`Band::index`]:
    /// index 0 = `Haystack` (innermost core), 1 = `RecentConcluded` (middle), 2 = `Hot` (outermost rim).
    pub band_angles: [BandAngleConfig; 3],
}

impl LayoutConfig {
    /// Default configuration: 300-unit radius, each band starts at 0 radians,
    /// pitch = `2π / 12` ≈ 30° — room for ~12 contexts per ring at a consistent
    /// angular spacing.
    pub fn default_config() -> Self {
        let pitch = std::f64::consts::TAU / 12.0;
        Self {
            total_radius: 300.0,
            band_angles: [
                BandAngleConfig { start_angle: 0.0, pitch },
                BandAngleConfig { start_angle: 0.0, pitch },
                BandAngleConfig { start_angle: 0.0, pitch },
            ],
        }
    }
}

// ─── ContextEntry ─────────────────────────────────────────────────────────────

/// One context record fed into [`CompactingBandLayout::compute`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextEntry<Id> {
    /// Stable identifier.
    pub id: Id,
    /// Pre-assigned lifecycle band.
    pub band: Band,
    /// Monotonically increasing key that determines within-band ordering.
    /// Lower = earlier = closer to the leading edge.
    pub order_key: i64,
}

// ─── CompactingBandLayout ────────────────────────────────────────────────────

/// Pure, stateless compacting-band layout for the time-well context browser.
///
/// See the [module documentation](self) for design rationale and the stateless finding.
///
/// # Usage
///
/// ```
/// use kaijutsu_viz::layout::{
///     Band, BandAngleConfig, CompactingBandLayout, ContextEntry, LayoutConfig,
/// };
///
/// let config = LayoutConfig::default_config();
/// let layout = CompactingBandLayout::new(config);
///
/// let entries = vec![
///     ContextEntry { id: 1u32, band: Band::Hot, order_key: 100 },
///     ContextEntry { id: 2u32, band: Band::Hot, order_key: 200 },
/// ];
///
/// let positions = layout.compute(&entries);
/// assert_eq!(positions.len(), 2);
/// ```
pub struct CompactingBandLayout {
    config: LayoutConfig,
    bands: RadialBands,
}

impl CompactingBandLayout {
    /// Construct a new layout with the given configuration.
    ///
    /// # Panics
    ///
    /// - `config.total_radius` ≤ 0.
    /// - Any `config.band_angles[i].pitch` ≤ 0.
    pub fn new(config: LayoutConfig) -> Self {
        assert!(
            config.total_radius > 0.0,
            "CompactingBandLayout: total_radius must be > 0, got {}",
            config.total_radius
        );
        assert!(
            config.total_radius.is_finite() && config.total_radius <= 1e6,
            "CompactingBandLayout: total_radius must be finite and ≤ 1e6, got {}",
            config.total_radius
        );
        for (i, ba) in config.band_angles.iter().enumerate() {
            assert!(
                ba.pitch > 0.0,
                "CompactingBandLayout: band_angles[{}].pitch must be > 0, got {}",
                i,
                ba.pitch
            );
        }
        let bands = RadialBands::new(config.total_radius, 3);
        Self { config, bands }
    }

    /// Compute a `LayoutPos` for every context entry.
    ///
    /// Within each band, contexts are ordered by `order_key` ascending and
    /// assigned a slot index `0, 1, 2, ...`.  Slot angle:
    ///
    /// ```text
    /// θ = band_start_angle + slot_index * pitch
    /// ```
    ///
    /// Radius is the mid-annulus of the band's ring:
    ///
    /// ```text
    /// r = RadialBands::radius(band.index(), 0.5)
    /// ```
    ///
    /// Cartesian position:
    ///
    /// ```text
    /// x = r * cos(θ)   (f64 → f32)
    /// y = r * sin(θ)   (f64 → f32)
    /// ```
    ///
    /// # Panics
    ///
    /// Duplicate ids in `entries` → panic naming the duplicate id.
    pub fn compute<Id>(&self, entries: &[ContextEntry<Id>]) -> BTreeMap<Id, LayoutPos>
    where
        Id: Clone + Ord + std::fmt::Debug,
    {
        // Fail-loud: detect duplicate ids before doing any work.
        {
            let mut seen: BTreeMap<&Id, ()> = BTreeMap::new();
            for e in entries {
                if seen.insert(&e.id, ()).is_some() {
                    panic!(
                        "CompactingBandLayout::compute: duplicate id {:?} in entries — data corruption",
                        e.id
                    );
                }
            }
        }

        // Group entries by band, preserving their ids for later lookup.
        // We sort each group by order_key ascending to establish slot indices.
        let mut by_band: [Vec<&ContextEntry<Id>>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        for e in entries {
            by_band[e.band.index()].push(e);
        }
        for group in by_band.iter_mut() {
            // Stable sort by order_key ascending.
            group.sort_by_key(|e| e.order_key);
        }

        let mut out: BTreeMap<Id, LayoutPos> = BTreeMap::new();

        for (band_index, group) in by_band.iter().enumerate() {
            let band_cfg = &self.config.band_angles[band_index];
            // Mid-annulus radius: fraction = 0.5 gives the clean ring.
            let r: f64 = self.bands.radius(band_index, 0.5);

            for (slot, entry) in group.iter().enumerate() {
                let theta: f64 = band_cfg.start_angle + slot as f64 * band_cfg.pitch;
                let x = (r * theta.cos()) as f32;
                let y = (r * theta.sin()) as f32;

                // Reconstruct Band from band_index.
                let band = match band_index {
                    0 => Band::Haystack,
                    1 => Band::RecentConcluded,
                    2 => Band::Hot,
                    _ => unreachable!("band_index is 0..3 by construction"),
                };

                out.insert(entry.id.clone(), LayoutPos { band, x, y });
            }
        }

        out
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::f64::consts::TAU;

    const F32_EPSILON: f32 = 1e-4;

    fn approx_eq_f32(a: f32, b: f32) -> bool {
        (a - b).abs() < F32_EPSILON
    }

    /// A config with deterministic angles for easy hand-calculation in tests.
    fn test_config(pitch_radians: f64) -> LayoutConfig {
        LayoutConfig {
            total_radius: 300.0,
            band_angles: [
                BandAngleConfig { start_angle: 0.0, pitch: pitch_radians },
                BandAngleConfig { start_angle: 0.0, pitch: pitch_radians },
                BandAngleConfig { start_angle: 0.0, pitch: pitch_radians },
            ],
        }
    }

    fn default_layout() -> CompactingBandLayout {
        CompactingBandLayout::new(LayoutConfig::default_config())
    }

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

    // ── CompactingBandLayout: config validation panics ─────────────────────

    #[test]
    #[should_panic(expected = "total_radius must be > 0")]
    fn layout_panics_on_zero_radius() {
        let mut cfg = LayoutConfig::default_config();
        cfg.total_radius = 0.0;
        let _ = CompactingBandLayout::new(cfg);
    }

    #[test]
    #[should_panic(expected = "total_radius must be > 0")]
    fn layout_panics_on_negative_radius() {
        let mut cfg = LayoutConfig::default_config();
        cfg.total_radius = -1.0;
        let _ = CompactingBandLayout::new(cfg);
    }

    #[test]
    #[should_panic(expected = "pitch must be > 0")]
    fn layout_panics_on_zero_pitch() {
        let mut cfg = LayoutConfig::default_config();
        cfg.band_angles[0].pitch = 0.0;
        let _ = CompactingBandLayout::new(cfg);
    }

    #[test]
    #[should_panic(expected = "pitch must be > 0")]
    fn layout_panics_on_negative_pitch() {
        let mut cfg = LayoutConfig::default_config();
        cfg.band_angles[1].pitch = -0.1;
        let _ = CompactingBandLayout::new(cfg);
    }

    // ── CompactingBandLayout: empty input ─────────────────────────────────

    #[test]
    fn empty_input_returns_empty_map() {
        let layout = default_layout();
        let entries: Vec<ContextEntry<u32>> = vec![];
        let positions = layout.compute(&entries);
        assert!(positions.is_empty(), "empty input must produce empty output");
    }

    // ── CompactingBandLayout: single context ──────────────────────────────

    #[test]
    fn single_hot_context_placed_at_slot_zero() {
        let pitch = TAU / 12.0;
        let layout = CompactingBandLayout::new(test_config(pitch));
        let entries = vec![ContextEntry { id: 1u32, band: Band::Hot, order_key: 0 }];
        let pos = layout.compute(&entries);

        let p = &pos[&1];
        assert_eq!(p.band, Band::Hot);

        // Slot 0: theta = start_angle (0.0) + 0 * pitch = 0.0
        // r = RadialBands(300, 3).radius(2, 0.5) = band_width * 2.5 = 100 * 2.5 = 250
        let expected_r = 250.0_f64;
        let expected_x = (expected_r * 0.0_f64.cos()) as f32;
        let expected_y = (expected_r * 0.0_f64.sin()) as f32;
        assert!(approx_eq_f32(p.x, expected_x), "x mismatch: got {}, expected {}", p.x, expected_x);
        assert!(approx_eq_f32(p.y, expected_y), "y mismatch: got {}, expected {}", p.y, expected_y);
    }

    // ── CompactingBandLayout: correct clean-ring radius per band ──────────

    #[test]
    fn each_band_uses_mid_annulus_radius() {
        // RadialBands(300, 3): band_width = 100.
        // Band 0 (Haystack): r = 0 * 100 + 0.5 * 100 = 50
        // Band 1 (RecentConcluded): r = 1 * 100 + 0.5 * 100 = 150
        // Band 2 (Hot): r = 2 * 100 + 0.5 * 100 = 250
        let layout = CompactingBandLayout::new(test_config(TAU / 12.0));
        let entries = vec![
            ContextEntry { id: 0u32, band: Band::Haystack,        order_key: 0 },
            ContextEntry { id: 1u32, band: Band::RecentConcluded, order_key: 0 },
            ContextEntry { id: 2u32, band: Band::Hot,             order_key: 0 },
        ];
        let pos = layout.compute(&entries);

        // At theta=0: x = r, y = 0.
        let haystack_r = (pos[&0].x.powi(2) + pos[&0].y.powi(2)).sqrt();
        let recent_r   = (pos[&1].x.powi(2) + pos[&1].y.powi(2)).sqrt();
        let hot_r      = (pos[&2].x.powi(2) + pos[&2].y.powi(2)).sqrt();

        assert!(approx_eq_f32(haystack_r, 50.0),  "Haystack mid-radius should be 50,  got {haystack_r}");
        assert!(approx_eq_f32(recent_r,   150.0), "RecentConcluded mid-radius should be 150, got {recent_r}");
        assert!(approx_eq_f32(hot_r,      250.0), "Hot mid-radius should be 250, got {hot_r}");
    }

    // ── CompactingBandLayout: fixed-pitch angle correctness ───────────────

    #[test]
    fn fixed_pitch_angle_assignment() {
        // 3 Hot contexts: order_keys 10, 20, 30.
        // Slot 0 → θ=0, slot 1 → θ=pitch, slot 2 → θ=2*pitch.
        let pitch = TAU / 8.0; // 45° steps for easy hand-check
        let layout = CompactingBandLayout::new(test_config(pitch));
        let entries = vec![
            ContextEntry { id: 1u32, band: Band::Hot, order_key: 30 }, // sorted → slot 2
            ContextEntry { id: 2u32, band: Band::Hot, order_key: 10 }, // sorted → slot 0
            ContextEntry { id: 3u32, band: Band::Hot, order_key: 20 }, // sorted → slot 1
        ];
        let pos = layout.compute(&entries);
        let r = 250.0_f64; // Hot mid-radius for total=300, 3 bands

        // id=2 (order_key=10) → slot 0 → θ=0
        let x0 = (r * 0.0_f64.cos()) as f32;
        let y0 = (r * 0.0_f64.sin()) as f32;
        assert!(approx_eq_f32(pos[&2].x, x0), "id=2 (slot 0) x mismatch");
        assert!(approx_eq_f32(pos[&2].y, y0), "id=2 (slot 0) y mismatch");

        // id=3 (order_key=20) → slot 1 → θ=pitch
        let x1 = (r * pitch.cos()) as f32;
        let y1 = (r * pitch.sin()) as f32;
        assert!(approx_eq_f32(pos[&3].x, x1), "id=3 (slot 1) x mismatch");
        assert!(approx_eq_f32(pos[&3].y, y1), "id=3 (slot 1) y mismatch");

        // id=1 (order_key=30) → slot 2 → θ=2*pitch
        let x2 = (r * (2.0 * pitch).cos()) as f32;
        let y2 = (r * (2.0 * pitch).sin()) as f32;
        assert!(approx_eq_f32(pos[&1].x, x2), "id=1 (slot 2) x mismatch");
        assert!(approx_eq_f32(pos[&1].y, y2), "id=1 (slot 2) y mismatch");
    }

    // ── CompactingBandLayout: polar→Cartesian sanity ──────────────────────

    #[test]
    fn slot_zero_is_at_start_angle() {
        // At start_angle=0, cos(0)=1, sin(0)=0 → x=r, y=0.
        let layout = CompactingBandLayout::new(test_config(TAU / 12.0));
        let entries = vec![ContextEntry { id: 42u32, band: Band::Hot, order_key: 0 }];
        let pos = layout.compute(&entries);
        let p = &pos[&42];
        assert!(approx_eq_f32(p.x, 250.0), "x should be r=250 at theta=0, got {}", p.x);
        assert!(approx_eq_f32(p.y, 0.0), "y should be 0 at theta=0, got {}", p.y);
    }

    #[test]
    fn slot_at_quarter_turn_has_expected_cartesian() {
        // pitch = π/2 → slot 1 is at θ=π/2 → x=0, y=r.
        let pitch = std::f64::consts::FRAC_PI_2;
        let layout = CompactingBandLayout::new(test_config(pitch));
        let entries = vec![
            ContextEntry { id: 0u32, band: Band::Haystack, order_key: 0 }, // slot 0 → θ=0
            ContextEntry { id: 1u32, band: Band::Haystack, order_key: 1 }, // slot 1 → θ=π/2
        ];
        let pos = layout.compute(&entries);
        let r_haystack = 50.0_f32; // 300/3 * 0.5
        // slot 1: x = r*cos(π/2) ≈ 0, y = r*sin(π/2) = r
        assert!(approx_eq_f32(pos[&1].x, 0.0), "x at π/2 should be ~0, got {}", pos[&1].x);
        assert!(approx_eq_f32(pos[&1].y, r_haystack), "y at π/2 should be {r_haystack}, got {}", pos[&1].y);
    }

    // ── CompactingBandLayout: full band ───────────────────────────────────

    #[test]
    fn full_band_all_positions_distinct() {
        let pitch = TAU / 20.0;
        let layout = CompactingBandLayout::new(test_config(pitch));
        let entries: Vec<ContextEntry<u32>> = (0..20)
            .map(|i| ContextEntry { id: i, band: Band::Hot, order_key: i as i64 })
            .collect();
        let pos = layout.compute(&entries);
        assert_eq!(pos.len(), 20);

        // All (x,y) pairs must be distinct (with a small tolerance).
        let positions: Vec<(f32, f32)> = pos.values().map(|p| (p.x, p.y)).collect();
        for i in 0..positions.len() {
            for j in (i + 1)..positions.len() {
                let dx = positions[i].0 - positions[j].0;
                let dy = positions[i].1 - positions[j].1;
                let dist = (dx * dx + dy * dy).sqrt();
                assert!(dist > 1.0, "positions {i} and {j} are too close: dist={dist}");
            }
        }
    }

    // ── CompactingBandLayout: duplicate id panics ─────────────────────────

    #[test]
    #[should_panic(expected = "duplicate id")]
    fn duplicate_id_panics() {
        let layout = default_layout();
        let entries = vec![
            ContextEntry { id: 1u32, band: Band::Hot, order_key: 0 },
            ContextEntry { id: 1u32, band: Band::Hot, order_key: 1 },
        ];
        let _ = layout.compute(&entries);
    }

    // ── CompactingBandLayout: determinism ─────────────────────────────────

    #[test]
    fn same_input_produces_same_output() {
        let layout = default_layout();
        let entries = vec![
            ContextEntry { id: 10u32, band: Band::Hot,             order_key: 50 },
            ContextEntry { id: 20u32, band: Band::RecentConcluded, order_key: 30 },
            ContextEntry { id: 30u32, band: Band::Haystack,        order_key: 10 },
        ];
        let pos1 = layout.compute(&entries);
        let pos2 = layout.compute(&entries);
        assert_eq!(pos1, pos2, "same input must produce identical output");
    }

    // ── Motion invariants ─────────────────────────────────────────────────
    //
    // These are the key correctness tests.  They are also exercised as proptests
    // below for broader coverage.

    /// INV-append: adding a context with the maximum order_key in its band leaves
    /// every existing context's LayoutPos byte-for-byte unchanged.
    #[test]
    fn inv_append_adding_max_order_key_moves_nothing() {
        let layout = CompactingBandLayout::new(test_config(TAU / 12.0));
        let before: Vec<ContextEntry<u32>> = vec![
            ContextEntry { id: 1, band: Band::Hot, order_key: 10 },
            ContextEntry { id: 2, band: Band::Hot, order_key: 20 },
            ContextEntry { id: 3, band: Band::Hot, order_key: 30 },
        ];
        let pos_before = layout.compute(&before);

        let after: Vec<ContextEntry<u32>> = {
            let mut v = before.clone();
            v.push(ContextEntry { id: 99, band: Band::Hot, order_key: 100 }); // new max
            v
        };
        let pos_after = layout.compute(&after);

        // Every existing context must be in the same position.
        for id in [1u32, 2, 3] {
            assert_eq!(
                pos_before[&id], pos_after[&id],
                "INV-append: id={id} moved after appending a new max-order_key context"
            );
        }
        // The new context is present.
        assert!(pos_after.contains_key(&99), "new context must appear in output");
    }

    /// INV-compact: removing one context moves only the contexts after it (higher
    /// order_key) in the same band — each shifts by one pitch — and moves nothing
    /// in any other band.
    #[test]
    fn inv_compact_removing_mid_context_shifts_later_only() {
        let pitch = TAU / 12.0;
        let layout = CompactingBandLayout::new(test_config(pitch));

        // Hot band: 4 contexts with ascending order_keys.
        // RecentConcluded band: 2 contexts — must be completely unaffected.
        let before: Vec<ContextEntry<u32>> = vec![
            ContextEntry { id: 1,  band: Band::Hot,             order_key: 10 },
            ContextEntry { id: 2,  band: Band::Hot,             order_key: 20 }, // remove this
            ContextEntry { id: 3,  band: Band::Hot,             order_key: 30 },
            ContextEntry { id: 4,  band: Band::Hot,             order_key: 40 },
            ContextEntry { id: 10, band: Band::RecentConcluded, order_key: 5  },
            ContextEntry { id: 11, band: Band::RecentConcluded, order_key: 15 },
        ];
        let pos_before = layout.compute(&before);

        // Remove id=2 (order_key=20).
        let after: Vec<ContextEntry<u32>> = before.iter().filter(|e| e.id != 2).cloned().collect();
        let pos_after = layout.compute(&after);

        // id=1 (order_key=10, slot 0 before and after) must NOT move.
        assert_eq!(
            pos_before[&1], pos_after[&1],
            "INV-compact: id=1 (before the removed context) must not move"
        );

        // id=3 was at slot 2, now at slot 1 → shifted by exactly -1 pitch.
        // id=4 was at slot 3, now at slot 2 → shifted by exactly -1 pitch.
        let r_hot = layout.bands.radius(Band::Hot.index(), 0.5); // derived from layout's own RadialBands
        let theta_slot = |s: usize| -> (f32, f32) {
            let theta = 0.0_f64 + s as f64 * pitch;
            ((r_hot * theta.cos()) as f32, (r_hot * theta.sin()) as f32)
        };

        let (x1_after, y1_after) = theta_slot(1);
        let (x2_after, y2_after) = theta_slot(2);
        assert!(approx_eq_f32(pos_after[&3].x, x1_after), "id=3 should be at slot 1 after compact");
        assert!(approx_eq_f32(pos_after[&3].y, y1_after), "id=3 should be at slot 1 after compact");
        assert!(approx_eq_f32(pos_after[&4].x, x2_after), "id=4 should be at slot 2 after compact");
        assert!(approx_eq_f32(pos_after[&4].y, y2_after), "id=4 should be at slot 2 after compact");

        // Other band (RecentConcluded) must be completely unaffected.
        assert_eq!(pos_before[&10], pos_after[&10], "INV-compact: other-band context id=10 must not move");
        assert_eq!(pos_before[&11], pos_after[&11], "INV-compact: other-band context id=11 must not move");
    }

    /// A context whose band/order_key is unchanged never moves when an unrelated
    /// context in a different band changes.
    #[test]
    fn unrelated_band_change_does_not_move_other_bands() {
        let layout = CompactingBandLayout::new(test_config(TAU / 12.0));
        let before: Vec<ContextEntry<u32>> = vec![
            ContextEntry { id: 1, band: Band::Hot,             order_key: 10 },
            ContextEntry { id: 2, band: Band::RecentConcluded, order_key: 5  },
            ContextEntry { id: 3, band: Band::Haystack,        order_key: 1  },
        ];
        let pos_before = layout.compute(&before);

        // Add a new RecentConcluded context — Hot and Haystack must be unchanged.
        let after: Vec<ContextEntry<u32>> = {
            let mut v = before.clone();
            v.push(ContextEntry { id: 99, band: Band::RecentConcluded, order_key: 50 });
            v
        };
        let pos_after = layout.compute(&after);

        assert_eq!(pos_before[&1], pos_after[&1], "Hot context must not move when RecentConcluded changes");
        assert_eq!(pos_before[&3], pos_after[&3], "Haystack context must not move when RecentConcluded changes");
    }

    // ── Property tests (proptests for motion invariants) ──────────────────

    #[cfg(test)]
    mod props {
        use super::*;
        use proptest::prelude::*;

        /// Generate a small set of unique ids with assigned bands and order_keys.
        /// Ids are in [0, 50), order_keys in [0, 1000), bands uniformly distributed.
        fn arb_context_entries(
            max_count: usize,
        ) -> impl Strategy<Value = Vec<ContextEntry<u32>>> {
            prop::collection::vec(
                (0u32..50u32, 0i64..1000i64, 0u8..3u8),
                0..max_count,
            )
            .prop_map(|raw| {
                // Deduplicate by id (keep last occurrence).
                let mut seen: BTreeMap<u32, (i64, u8)> = BTreeMap::new();
                for (id, ok, band_idx) in raw {
                    seen.insert(id, (ok, band_idx));
                }
                seen.into_iter()
                    .map(|(id, (ok, band_idx))| {
                        let band = match band_idx % 3 {
                            0 => Band::Haystack,
                            1 => Band::RecentConcluded,
                            _ => Band::Hot,
                        };
                        ContextEntry { id, band, order_key: ok }
                    })
                    .collect()
            })
        }

        fn make_layout() -> CompactingBandLayout {
            CompactingBandLayout::new(LayoutConfig::default_config())
        }

        // PROP-APPEND: appending a context with max order_key in its band moves no
        // existing context.
        proptest! {
            #[test]
            fn prop_inv_append(entries in arb_context_entries(15)) {
                let layout = make_layout();
                if entries.is_empty() {
                    return Ok(());
                }
                let pos_before = layout.compute(&entries);

                // Find the maximum order_key across all entries, then add 1.
                let max_ok = entries.iter().map(|e| e.order_key).max().unwrap_or(0);
                // Pick an id that is not already present.
                let new_id = (0u32..1000).find(|i| !pos_before.contains_key(i)).unwrap();

                // Pick a band at random (use new_id % 3 as a deterministic choice so
                // this test is repeatable even though proptest controls new_id).
                let new_band = match new_id % 3 {
                    0 => Band::Haystack,
                    1 => Band::RecentConcluded,
                    _ => Band::Hot,
                };

                let mut after = entries.clone();
                after.push(ContextEntry { id: new_id, band: new_band, order_key: max_ok + 1 });
                let pos_after = layout.compute(&after);

                for e in &entries {
                    // Only contexts in the SAME band at or below the new max are
                    // guaranteed to be unmoved.  Here new_id's order_key is strictly
                    // greater than all existing, so all existing contexts keep their slot.
                    prop_assert_eq!(
                        &pos_before[&e.id], &pos_after[&e.id],
                        "INV-append violated for id={}", e.id
                    );
                }
            }
        }

        // PROP-COMPACT: removing one context moves only later-order_key contexts in the
        // same band — each shifts back by exactly one pitch — and contexts in other bands
        // are untouched.
        //
        // "Later" is defined by the spec: same band, order_key strictly greater than the
        // removed entry's order_key.  We do NOT re-run compute's sort to define it.
        // To keep the oracle unambiguous, we skip cases where another same-band entry
        // ties the removed entry's order_key (stable-sort position would determine slot
        // assignment, which IS implementation-dependent).
        proptest! {
            #[test]
            fn prop_inv_compact(entries in arb_context_entries(15)) {
                // Need at least 2 entries to have something to remove and something to observe.
                prop_assume!(entries.len() >= 2);

                let layout = make_layout();
                let pos_before = layout.compute(&entries);

                // ── choose the entry to remove ──────────────────────────────────────
                // Pick the entry with median overall sort position so there are entries
                // both before and after it globally.
                let mut sorted_global: Vec<&ContextEntry<u32>> = entries.iter().collect();
                sorted_global.sort_by_key(|e| e.order_key);
                let mid_idx = sorted_global.len() / 2;
                let removed = sorted_global[mid_idx];

                // ── guard: skip if another same-band entry ties on order_key ──────
                // A tie means stable-sort order (= input order) resolves the slot,
                // which is an impl detail, not the spec.  Skip those cases.
                let same_band_tie = entries.iter().any(|e| {
                    e.id != removed.id && e.band == removed.band && e.order_key == removed.order_key
                });
                prop_assume!(!same_band_tie);

                let after: Vec<ContextEntry<u32>> = entries.iter()
                    .filter(|e| e.id != removed.id)
                    .cloned()
                    .collect();
                let pos_after = layout.compute(&after);

                let removed_band_index = removed.band.index();
                let pitch = layout.config.band_angles[removed_band_index].pitch;

                for e in &entries {
                    if e.id == removed.id {
                        prop_assert!(!pos_after.contains_key(&e.id),
                            "removed id={} must not appear in output", e.id);
                        continue;
                    }

                    if e.band != removed.band {
                        // Different band — must not move at all (direct position equality).
                        prop_assert_eq!(
                            &pos_before[&e.id], &pos_after[&e.id],
                            "INV-compact: id={} in different band must not move", e.id
                        );
                        continue;
                    }

                    if e.order_key < removed.order_key {
                        // Before the removed entry in spec order — must not move.
                        prop_assert_eq!(
                            &pos_before[&e.id], &pos_after[&e.id],
                            "INV-compact: id={} (before removed, order_key {} < {}) must not move",
                            e.id, e.order_key, removed.order_key
                        );
                    } else {
                        // After the removed entry in spec order (order_key strictly greater,
                        // ties excluded above) — each shifts back by exactly one pitch.
                        // Geometric check: rotating pos_after by +pitch must recover pos_before.
                        let pos_before_e = &pos_before[&e.id];
                        let pos_after_e  = &pos_after[&e.id];
                        let cos_p = pitch.cos() as f32;
                        let sin_p = pitch.sin() as f32;
                        let x_rot = pos_after_e.x * cos_p - pos_after_e.y * sin_p;
                        let y_rot = pos_after_e.x * sin_p + pos_after_e.y * cos_p;
                        prop_assert!(
                            (x_rot - pos_before_e.x).abs() < 1e-3,
                            "INV-compact geometric x: id={} rotate(pos_after, +pitch).x={} expected pos_before.x={}",
                            e.id, x_rot, pos_before_e.x
                        );
                        prop_assert!(
                            (y_rot - pos_before_e.y).abs() < 1e-3,
                            "INV-compact geometric y: id={} rotate(pos_after, +pitch).y={} expected pos_before.y={}",
                            e.id, y_rot, pos_before_e.y
                        );
                    }
                }
            }
        }

        // PROP-DETERMINISM: same input always yields the same output.
        proptest! {
            #[test]
            fn prop_determinism(entries in arb_context_entries(20)) {
                let layout = make_layout();
                let pos1 = layout.compute(&entries);
                let pos2 = layout.compute(&entries);
                prop_assert_eq!(pos1, pos2, "layout must be deterministic");
            }
        }

        // PROP-NO-CROSS-BAND-CONTAMINATION: adding or removing contexts in one band
        // never changes positions in other bands.
        proptest! {
            #[test]
            fn prop_cross_band_isolation(entries in arb_context_entries(15)) {
                let layout = make_layout();
                if entries.is_empty() {
                    return Ok(());
                }
                let pos_before = layout.compute(&entries);

                // Add a new Hot context with a max+1 order_key.
                let max_ok = entries.iter().map(|e| e.order_key).max().unwrap_or(0);
                let new_id = (0u32..1000).find(|i| !pos_before.contains_key(i)).unwrap();

                let mut after = entries.clone();
                after.push(ContextEntry { id: new_id, band: Band::Hot, order_key: max_ok + 1 });
                let pos_after = layout.compute(&after);

                // Contexts in non-Hot bands must be unmoved.
                for e in entries.iter().filter(|e| e.band != Band::Hot) {
                    prop_assert_eq!(
                        &pos_before[&e.id], &pos_after[&e.id],
                        "cross-band contamination: non-Hot id={} moved when Hot got a new entry", e.id
                    );
                }
            }
        }

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
