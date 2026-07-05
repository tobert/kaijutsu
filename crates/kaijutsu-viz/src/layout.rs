//! Explicit ring-seat placement for the time-well context browser.
//!
//! # Architecture
//!
//! Ring membership is **explicit placement**, not derived idle-age banding.
//! The idle-age scheme (`HOT_NOW_MILLIS`/`THIS_WEEK_MILLIS`/`THIRTY_DAYS_MILLIS`,
//! the `running` override, and the conclude-demotes-one-band rule) lasted two
//! days in live use and is superseded — see `docs/timewell.md`, "Ring
//! membership becomes explicit." [`assign_ring_seats`] is the pure core: given
//! every context's lifecycle stamps, it seats up to [`RING_SLOTS`] (10) ids
//! per ring and returns everything left over as an unseated `horizon` list.
//! Four rings, mouth → throat:
//!
//! - **[`Band::Active`]** (ring 0, explicit) — `promoted_at` set. Ordered
//!   `promoted_at` ascending (append order, so a seat holds its position for
//!   as long as it's occupied — "stable seats"). The kernel enforces the
//!   10-seat cap (`ACTIVE_RING_CAPACITY` in `kernel_db.rs`); this fn degrades
//!   sanely if that invariant is ever violated (rule 2 below).
//! - **[`Band::Recent`]** (ring 1, automatic) — the 10 most-recently-active
//!   non-concluded contexts of the remaining ("auto") pool.
//! - **[`Band::Bumped`]** (ring 2, automatic overflow) — the next 10 of the
//!   same pool by the same recency order; concluded contexts compete *only*
//!   here, never for `Recent`.
//! - **[`Band::Demoted`]** (ring 3, explicit) — `demoted_at` set. Ordered
//!   `demoted_at` descending (most recently demoted first).
//! - **event horizon** — everything past seat 9 of any ring: no card entity,
//!   just an unseated id the caller renders as a "+N" count at the throat.
//!
//! There is no `now` parameter and no age math anywhere in this module —
//! placement depends only on each context's own stamps and its rank among
//! the others.
//!
//! This module previously also carried `CompactingBandLayout`, a pure polar
//! layout engine built on `RadialBands` — deleted 2026-07-03 (no consumer
//! since the 7.8 spiral superseded the ring layout; see `docs/timewell.md`,
//! "Where we are" / "Dead code"). `RadialBands` (`crate::scales`) stays for
//! now, unused by this module (the terracing in `kaijutsu-app`'s `card.rs`
//! divides its own radius/depth envelope directly rather than going through
//! it).
//!
//! # Fail-loud stance
//!
//! No silent clamping. Two invariants the kernel is expected to hold are
//! checked with `debug_assert!` (compiled out in release, where the well
//! degrades sanely instead of crashing live):
//!
//! 1. A context should never carry both `promoted_at` and `demoted_at` — the
//!    kernel clears one when it sets the other. If it ever happens anyway,
//!    demoted wins (rule 1 below is checked first, unconditionally).
//! 2. Ring 0 should never receive more than [`RING_SLOTS`] promotions (the
//!    kernel enforces `ACTIVE_RING_CAPACITY`) — an overflow spills to the
//!    horizon rather than silently dropping ids or growing the ring past its
//!    10 seats.

// ─── Band ────────────────────────────────────────────────────────────────────

/// Which ring a context is seated on.
///
/// Declared mouth→throat so the derived [`Ord`] (used nowhere load-bearing
/// today, kept for free) agrees with [`Band::index`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Band {
    /// Ring 0, explicit: `promoted_at` set. Mouth (index 0).
    Active,
    /// Ring 1, automatic: the most-recently-active non-concluded contexts.
    Recent,
    /// Ring 2, automatic overflow from `Recent`; concluded contexts compete
    /// only here.
    Bumped,
    /// Ring 3, explicit: `demoted_at` set. Throat (index 3).
    Demoted,
}

impl Band {
    /// Numeric band index, mouth→throat: `Active` = 0 (shallowest) …
    /// `Demoted` = 3 (deepest). The single source `card.rs`'s terracing and
    /// slot-order code key off — see `docs/timewell.md`, "The bowl, revisited".
    pub fn index(self) -> usize {
        match self {
            Band::Active => 0,
            Band::Recent => 1,
            Band::Bumped => 2,
            Band::Demoted => 3,
        }
    }
}

/// All four bands, mouth→throat. The single source for "walk every band in
/// order" call sites (`card.rs`'s ring-seat/geometry code).
pub const ALL_BANDS: [Band; 4] = [Band::Active, Band::Recent, Band::Bumped, Band::Demoted];

// ─── assign_ring_seats ──────────────────────────────────────────────────────

/// Seats per ring. Ring 0's cap is **also** enforced kernel-side
/// (`ACTIVE_RING_CAPACITY` in `kernel_db.rs`). The canonical value is
/// `kaijutsu_types::RING_SLOTS` — this crate is deliberately zero-dep, so it
/// carries its own literal; a unit test in `kaijutsu-app` (which sees both
/// crates) pins the equality.
pub const RING_SLOTS: usize = 10;

/// Context descriptor used by [`assign_ring_seats`].
///
/// `Id` needs `Clone` (seat vectors own their ids), `Ord` (recency/timestamp
/// tie-breaks — seating ranks contexts against each other, unlike the old
/// per-context-only `assign_idle_band`), and `Debug` (fail-loud diagnostics).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextLifecycle<Id> {
    /// Stable context identifier.
    pub id: Id,
    /// Unix-millisecond creation time. Informational only — not read by
    /// [`assign_ring_seats`]; kept so a caller that coalesces
    /// `last_activity_at` to `created_at` for a never-touched context has
    /// somewhere to carry it (see `card.rs::effective_activity`).
    pub created_at: i64,
    /// Unix-millisecond conclusion time, or `None` if still open. A concluded
    /// context competes only for [`Band::Bumped`], never [`Band::Recent`]
    /// (rule 3 on [`assign_ring_seats`]).
    pub concluded_at: Option<i64>,
    /// Unix-millisecond timestamp of the most recent block activity — the
    /// recency key for the auto pool (rule 3).
    pub last_activity_at: i64,
    /// Unix-millisecond explicit ring-0 promote, or `None`. Mutually
    /// exclusive with `demoted_at` (see the module's fail-loud stance); ranks
    /// [`Band::Active`] ascending (append order).
    pub promoted_at: Option<i64>,
    /// Unix-millisecond explicit push to the demoted ring, or `None`. Ranks
    /// [`Band::Demoted`] descending (most recently demoted first).
    pub demoted_at: Option<i64>,
}

/// One ring per band ([`Band::index`]) plus the unseated overflow.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WellPlacement<Id> {
    /// Each ring's seated ids, in seat order (seat 0 first), indexed by
    /// [`Band::index`]. Never longer than [`RING_SLOTS`].
    pub rings: [Vec<Id>; 4],
    /// Ids that fit no ring — the event horizon. No seat order is promised;
    /// render as a "+N" count, never as cards.
    pub horizon: Vec<Id>,
}

/// Assign every context a ring seat (or the horizon), purely from each
/// context's own lifecycle stamps and its rank among the others.
///
/// Rules, in order:
/// 1. **Demoted**: `demoted_at.is_some()` → [`Band::Demoted`] candidates,
///    ranked `demoted_at` descending (id tiebreak). First [`RING_SLOTS`]
///    seated; the rest spill to the horizon — an ordinary overflow (nothing
///    else competes for this ring, so no `debug_assert` here).
/// 2. **Active**: of what's left, `promoted_at.is_some()` → [`Band::Active`]
///    candidates, ranked `promoted_at` ascending (id tiebreak) — append
///    order, so a seat holds its position as long as it's occupied. The
///    kernel caps this at [`RING_SLOTS`] (`ACTIVE_RING_CAPACITY`); if more
///    somehow arrive, this fn still seats only the first [`RING_SLOTS`] and
///    spills the rest to the horizon, `debug_assert`ing loud in dev (a kernel
///    bug) while degrading sanely live.
/// 3. **Auto pool** (the remainder — neither promoted nor demoted): ranked
///    `last_activity_at` descending (id tiebreak). Non-concluded contexts
///    compete for [`Band::Recent`]'s [`RING_SLOTS`] seats; everyone left over
///    (every concluded context, plus any non-concluded overflow) competes for
///    [`Band::Bumped`]'s [`RING_SLOTS`] seats in the same recency order (a
///    stable subsequence of an already-sorted list stays sorted); the rest
///    spill to the horizon.
///
/// A context should never carry both `promoted_at` and `demoted_at` — the
/// kernel clears one when it sets the other. `debug_assert`ed (fail loud in
/// dev); if it ever happens anyway, demoted wins (rule 1 is checked first,
/// unconditionally).
pub fn assign_ring_seats<Id>(contexts: &[ContextLifecycle<Id>]) -> WellPlacement<Id>
where
    Id: Clone + std::fmt::Debug + Eq + Ord,
{
    let mut demoted_candidates: Vec<&ContextLifecycle<Id>> = Vec::new();
    let mut active_candidates: Vec<&ContextLifecycle<Id>> = Vec::new();
    let mut auto_pool: Vec<&ContextLifecycle<Id>> = Vec::new();

    for c in contexts {
        debug_assert!(
            !(c.promoted_at.is_some() && c.demoted_at.is_some()),
            "context {:?} has both promoted_at and demoted_at set — the kernel \
             should clear one when it sets the other; demoted wins here",
            c.id
        );
        if c.demoted_at.is_some() {
            demoted_candidates.push(c);
        } else if c.promoted_at.is_some() {
            active_candidates.push(c);
        } else {
            auto_pool.push(c);
        }
    }

    let mut rings: [Vec<Id>; 4] = [Vec::new(), Vec::new(), Vec::new(), Vec::new()];
    let mut horizon: Vec<Id> = Vec::new();

    // Rule 1: Demoted, demoted_at descending, id tiebreak.
    demoted_candidates.sort_by(|a, b| b.demoted_at.cmp(&a.demoted_at).then_with(|| a.id.cmp(&b.id)));
    let (seated, overflow) = seat(demoted_candidates, RING_SLOTS);
    rings[Band::Demoted.index()] = seated;
    horizon.extend(overflow);

    // Rule 2: Active, promoted_at ascending (append order), id tiebreak.
    active_candidates.sort_by(|a, b| a.promoted_at.cmp(&b.promoted_at).then_with(|| a.id.cmp(&b.id)));
    debug_assert!(
        active_candidates.len() <= RING_SLOTS,
        "{} contexts are promoted at once — the kernel's ACTIVE_RING_CAPACITY \
         should cap this at {RING_SLOTS}; spilling the overflow to the horizon",
        active_candidates.len()
    );
    let (seated, overflow) = seat(active_candidates, RING_SLOTS);
    rings[Band::Active.index()] = seated;
    horizon.extend(overflow);

    // Rule 3: auto pool, last_activity_at descending, id tiebreak. Walking in
    // that order, non-concluded contexts fill Recent first; everything else
    // (concluded, plus any non-concluded overflow once Recent is full) falls
    // through to the Bumped pool, which — as a stable subsequence of the
    // recency-sorted walk — is already in recency order, no re-sort needed.
    auto_pool.sort_by(|a, b| b.last_activity_at.cmp(&a.last_activity_at).then_with(|| a.id.cmp(&b.id)));
    let mut recent: Vec<&ContextLifecycle<Id>> = Vec::new();
    let mut bumped_pool: Vec<&ContextLifecycle<Id>> = Vec::new();
    for c in auto_pool {
        if c.concluded_at.is_none() && recent.len() < RING_SLOTS {
            recent.push(c);
        } else {
            bumped_pool.push(c);
        }
    }
    rings[Band::Recent.index()] = recent.into_iter().map(|c| c.id.clone()).collect();
    let (seated, overflow) = seat(bumped_pool, RING_SLOTS);
    rings[Band::Bumped.index()] = seated;
    horizon.extend(overflow);

    WellPlacement { rings, horizon }
}

/// Split `candidates` (already ranked in seat order) into the first `n` seated
/// ids and the rest, spilled to the horizon as ids. A thin helper so the three
/// rings that seat-then-spill (`Demoted`, `Active`, `Bumped`) share one line.
fn seat<Id: Clone>(mut candidates: Vec<&ContextLifecycle<Id>>, n: usize) -> (Vec<Id>, Vec<Id>) {
    let overflow = if candidates.len() > n {
        candidates.split_off(n)
    } else {
        Vec::new()
    };
    (
        candidates.into_iter().map(|c| c.id.clone()).collect(),
        overflow.into_iter().map(|c| c.id.clone()).collect(),
    )
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn auto_ctx(id: u32, activity: i64, concluded: bool) -> ContextLifecycle<u32> {
        ContextLifecycle {
            id,
            created_at: 0,
            concluded_at: if concluded { Some(activity) } else { None },
            last_activity_at: activity,
            promoted_at: None,
            demoted_at: None,
        }
    }

    fn promoted_ctx(id: u32, promoted_at: i64) -> ContextLifecycle<u32> {
        ContextLifecycle {
            id,
            created_at: 0,
            concluded_at: None,
            last_activity_at: 0,
            promoted_at: Some(promoted_at),
            demoted_at: None,
        }
    }

    fn demoted_ctx(id: u32, demoted_at: i64) -> ContextLifecycle<u32> {
        ContextLifecycle {
            id,
            created_at: 0,
            concluded_at: None,
            last_activity_at: 0,
            promoted_at: None,
            demoted_at: Some(demoted_at),
        }
    }

    // ── Band::index / ALL_BANDS ─────────────────────────────────────────────

    #[test]
    fn band_index_is_mouth_to_throat() {
        assert_eq!(Band::Active.index(), 0, "Active is the mouth");
        assert_eq!(Band::Recent.index(), 1);
        assert_eq!(Band::Bumped.index(), 2);
        assert_eq!(Band::Demoted.index(), 3, "Demoted is the throat");
    }

    #[test]
    fn all_bands_is_mouth_to_throat_order() {
        assert_eq!(ALL_BANDS, [Band::Active, Band::Recent, Band::Bumped, Band::Demoted]);
    }

    // ── Empty input ──────────────────────────────────────────────────────────

    #[test]
    fn empty_input_yields_empty_everything() {
        let placement = assign_ring_seats::<u32>(&[]);
        for ring in &placement.rings {
            assert!(ring.is_empty());
        }
        assert!(placement.horizon.is_empty());
    }

    // ── Rule 1: Demoted ──────────────────────────────────────────────────────

    #[test]
    fn demoted_orders_most_recently_demoted_first() {
        let contexts = vec![demoted_ctx(1, 100), demoted_ctx(2, 300), demoted_ctx(3, 200)];
        let placement = assign_ring_seats(&contexts);
        assert_eq!(
            placement.rings[Band::Demoted.index()],
            vec![2, 3, 1],
            "demoted_at descending: most recently demoted first"
        );
        assert!(placement.horizon.is_empty());
    }

    #[test]
    fn demoted_overflow_past_ring_slots_spills_to_horizon() {
        // 12 demoted contexts, demoted_at = id so higher id = more recently demoted.
        let contexts: Vec<ContextLifecycle<u32>> =
            (1..=12u32).map(|i| demoted_ctx(i, i as i64)).collect();
        let placement = assign_ring_seats(&contexts);
        let seated = &placement.rings[Band::Demoted.index()];
        assert_eq!(seated.len(), RING_SLOTS, "only the first 10 seats fill");
        assert_eq!(seated, &vec![12, 11, 10, 9, 8, 7, 6, 5, 4, 3], "most recent 10 seated");
        assert_eq!(placement.horizon.len(), 2, "the oldest 2 demotions spill to the horizon");
        for id in [1u32, 2] {
            assert!(placement.horizon.contains(&id), "id {id} should be in the horizon");
        }
    }

    // ── Rule 2: Active ───────────────────────────────────────────────────────

    #[test]
    fn active_orders_append_order_ascending_promoted_at() {
        let contexts = vec![promoted_ctx(1, 300), promoted_ctx(2, 100), promoted_ctx(3, 200)];
        let placement = assign_ring_seats(&contexts);
        assert_eq!(
            placement.rings[Band::Active.index()],
            vec![2, 3, 1],
            "promoted_at ascending: first promoted holds seat 0"
        );
    }

    /// Seat stability: re-sorting/reordering the *input* array must not change
    /// the seat order — it's derived purely from `promoted_at`, not array
    /// position.
    #[test]
    fn active_seat_order_is_stable_under_input_reordering() {
        let forward = vec![promoted_ctx(1, 100), promoted_ctx(2, 200), promoted_ctx(3, 300)];
        let mut reversed = forward.clone();
        reversed.reverse();
        let mut shuffled = forward.clone();
        shuffled.swap(0, 2);

        let a = assign_ring_seats(&forward).rings[Band::Active.index()].clone();
        let b = assign_ring_seats(&reversed).rings[Band::Active.index()].clone();
        let c = assign_ring_seats(&shuffled).rings[Band::Active.index()].clone();

        assert_eq!(a, vec![1, 2, 3]);
        assert_eq!(a, b, "seat order survives a fully-reversed input");
        assert_eq!(a, c, "seat order survives an arbitrary input shuffle");
    }

    /// `#[should_panic]` documents the "fail loud in dev" half of rule 2: in a
    /// debug build (which `cargo test` is), an active-ring overflow the kernel
    /// should have prevented trips the `debug_assert` rather than silently
    /// seating 11+ contexts. (In release, `debug_assert!` compiles out and the
    /// fn instead seats the first `RING_SLOTS` and spills the rest — see
    /// `active_orders_append_order_ascending_promoted_at` for the ordering
    /// this degrade path reuses.) Gated to debug: under `--release` there is
    /// no assert to trip, so the expected panic never fires.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "ACTIVE_RING_CAPACITY")]
    fn promoted_overflow_trips_the_debug_assert() {
        let contexts: Vec<ContextLifecycle<u32>> =
            (1..=(RING_SLOTS as u32 + 1)).map(|i| promoted_ctx(i, i as i64)).collect();
        assign_ring_seats(&contexts);
    }

    /// The other documented invariant: a context should never carry both
    /// stamps. `#[should_panic]` for the same "fail loud in dev" reason as
    /// above; debug-gated for the same reason too. The release half —
    /// demoted-wins with seat conservation — is
    /// `props::both_stamps_degrades_to_demoted_wins_with_conservation`.
    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "has both promoted_at and demoted_at set")]
    fn both_stamps_set_trips_the_debug_assert() {
        let pathological = ContextLifecycle {
            id: 1u32,
            created_at: 0,
            concluded_at: None,
            last_activity_at: 0,
            promoted_at: Some(1),
            demoted_at: Some(1),
        };
        assign_ring_seats(&[pathological]);
    }

    // ── Rule 3: auto pool (Recent / Bumped) ──────────────────────────────────

    #[test]
    fn recent_orders_most_recently_active_first() {
        let contexts = vec![auto_ctx(1, 100, false), auto_ctx(2, 300, false), auto_ctx(3, 200, false)];
        let placement = assign_ring_seats(&contexts);
        assert_eq!(placement.rings[Band::Recent.index()], vec![2, 3, 1]);
        assert!(placement.rings[Band::Bumped.index()].is_empty());
    }

    #[test]
    fn concluded_is_excluded_from_recent_but_seated_in_bumped_by_recency() {
        // id 1 is concluded and the MOST recently active — it must not appear
        // in Recent, but it should still lead Bumped (recency order preserved).
        let contexts = vec![
            auto_ctx(1, 500, true),  // concluded, most recent -> Bumped seat 0
            auto_ctx(2, 300, false), // open -> Recent
            auto_ctx(3, 400, false), // open -> Recent
        ];
        let placement = assign_ring_seats(&contexts);
        assert_eq!(
            placement.rings[Band::Recent.index()],
            vec![3, 2],
            "Recent excludes the concluded context even though it's the most recent"
        );
        assert_eq!(
            placement.rings[Band::Bumped.index()],
            vec![1],
            "the concluded context is seated in Bumped instead"
        );
    }

    #[test]
    fn auto_pool_overflow_past_recent_spills_into_bumped_in_recency_order() {
        // 15 open (non-concluded) contexts: newest 10 -> Recent, next 5 -> Bumped.
        let contexts: Vec<ContextLifecycle<u32>> =
            (1..=15u32).map(|i| auto_ctx(i, i as i64, false)).collect();
        let placement = assign_ring_seats(&contexts);
        let recent = &placement.rings[Band::Recent.index()];
        let bumped = &placement.rings[Band::Bumped.index()];
        assert_eq!(recent.len(), RING_SLOTS);
        assert_eq!(recent, &vec![15, 14, 13, 12, 11, 10, 9, 8, 7, 6], "the 10 most recent");
        assert_eq!(bumped, &vec![5, 4, 3, 2, 1], "recency-ordered overflow, no re-sort surprises");
        assert!(placement.horizon.is_empty());
    }

    #[test]
    fn auto_pool_overflow_past_bumped_spills_to_the_horizon() {
        // 25 open contexts: 10 Recent, 10 Bumped, 5 horizon.
        let contexts: Vec<ContextLifecycle<u32>> =
            (1..=25u32).map(|i| auto_ctx(i, i as i64, false)).collect();
        let placement = assign_ring_seats(&contexts);
        assert_eq!(placement.rings[Band::Recent.index()].len(), RING_SLOTS);
        assert_eq!(placement.rings[Band::Bumped.index()].len(), RING_SLOTS);
        assert_eq!(placement.horizon.len(), 5);
        for id in 1u32..=5 {
            assert!(placement.horizon.contains(&id), "the oldest 5 spill to the horizon");
        }
    }

    // ── Ties ─────────────────────────────────────────────────────────────────

    #[test]
    fn ties_break_on_id_ascending() {
        // Same last_activity_at -> lower id wins the earlier (more mouth-ward) seat.
        let contexts = vec![auto_ctx(5, 100, false), auto_ctx(2, 100, false), auto_ctx(9, 100, false)];
        let placement = assign_ring_seats(&contexts);
        assert_eq!(
            placement.rings[Band::Recent.index()],
            vec![2, 5, 9],
            "equal recency ties break on ascending id"
        );
    }

    #[test]
    fn demoted_ties_break_on_id_ascending() {
        let contexts = vec![demoted_ctx(5, 100), demoted_ctx(2, 100), demoted_ctx(9, 100)];
        let placement = assign_ring_seats(&contexts);
        assert_eq!(placement.rings[Band::Demoted.index()], vec![2, 5, 9]);
    }

    // ── Combined scenario ────────────────────────────────────────────────────

    #[test]
    fn all_four_rules_compose_without_cross_talk() {
        let contexts = vec![
            promoted_ctx(1, 10),      // Active
            demoted_ctx(2, 10),       // Demoted
            auto_ctx(3, 500, false),  // Recent
            auto_ctx(4, 500, true),   // concluded, same recency -> Bumped only
        ];
        let placement = assign_ring_seats(&contexts);
        assert_eq!(placement.rings[Band::Active.index()], vec![1]);
        assert_eq!(placement.rings[Band::Demoted.index()], vec![2]);
        assert_eq!(placement.rings[Band::Recent.index()], vec![3]);
        assert_eq!(placement.rings[Band::Bumped.index()], vec![4]);
        assert!(placement.horizon.is_empty());
    }

    // ── Property test: conservation ─────────────────────────────────────────

    #[cfg(test)]
    mod props {
        use super::*;
        use proptest::prelude::*;
        use std::collections::HashSet;

        // PROP-SEAT-CONSERVATION: every input context ends up in exactly one
        // place (a ring seat or the horizon) — never duplicated, never
        // dropped, and no ring ever exceeds its RING_SLOTS cap. The most
        // important invariant this refactor could silently break.
        //
        // `promoted_ts` is capped at RING_SLOTS entries so it never trips the
        // rule-2 debug_assert (that's exercised deliberately, and separately,
        // by `promoted_overflow_trips_the_debug_assert` above); demoted counts
        // are uncapped since Demoted has no such cap.
        //
        // Timestamps draw from a deliberately dense 0..10 range so equal
        // stamps actually occur — the id-tiebreak arms of every sort are
        // exercised by the property, not just by the hand-written tie tests.
        proptest! {
            #[test]
            fn prop_every_context_is_seated_or_horizoned_exactly_once(
                promoted_ts in prop::collection::vec(0..10i64, 0..=RING_SLOTS),
                others in prop::collection::vec(
                    (0..10i64, prop::option::of(0..10i64), prop::bool::ANY, 0..10i64),
                    0..30,
                ),
            ) {
                let mut contexts: Vec<ContextLifecycle<u32>> = Vec::new();
                let mut next_id = 0u32;
                for ts in promoted_ts {
                    contexts.push(promoted_ctx(next_id, ts));
                    next_id += 1;
                }
                for (activity, concluded, is_demoted, ts) in others {
                    contexts.push(if is_demoted {
                        demoted_ctx(next_id, ts)
                    } else {
                        ContextLifecycle {
                            id: next_id,
                            created_at: 0,
                            concluded_at: concluded,
                            last_activity_at: activity,
                            promoted_at: None,
                            demoted_at: None,
                        }
                    });
                    next_id += 1;
                }

                let placement = assign_ring_seats(&contexts);

                let mut seen: HashSet<u32> = HashSet::new();
                for ring in &placement.rings {
                    prop_assert!(ring.len() <= RING_SLOTS, "a ring exceeded its {} seat cap", RING_SLOTS);
                    for id in ring {
                        prop_assert!(seen.insert(*id), "id {} seated twice", id);
                    }
                }
                for id in &placement.horizon {
                    prop_assert!(seen.insert(*id), "id {} double-counted (horizon + ring, or horizon twice)", id);
                }
                prop_assert_eq!(seen.len(), contexts.len(), "every context must be seated or horizoned exactly once");
            }
        }

        /// The release-mode half of the both-stamps invariant: with
        /// `debug_assert!` compiled out, a pathological both-stamps context
        /// (which the proptest generator's either/or branch can never
        /// produce) must degrade sanely — demoted wins, and seat
        /// conservation holds. Gated to release because in a debug build
        /// (what `cargo test` runs) the same input trips the assert instead
        /// — that half is covered by `both_stamps_set_trips_the_debug_assert`
        /// above. Run with `cargo test --release -p kaijutsu-viz`.
        #[cfg(not(debug_assertions))]
        #[test]
        fn both_stamps_degrades_to_demoted_wins_with_conservation() {
            let mut contexts = vec![
                promoted_ctx(1, 5),
                demoted_ctx(2, 5),
                auto_ctx(3, 5, false),
            ];
            contexts.push(ContextLifecycle {
                id: 4u32,
                created_at: 0,
                concluded_at: None,
                last_activity_at: 0,
                promoted_at: Some(9),
                demoted_at: Some(9),
            });

            let placement = assign_ring_seats(&contexts);

            assert!(
                placement.rings[Band::Demoted.index()].contains(&4),
                "demoted wins on a both-stamps row"
            );
            assert!(
                !placement.rings[Band::Active.index()].contains(&4),
                "a both-stamps row must not also take a ring-0 seat"
            );

            let mut seen: HashSet<u32> = HashSet::new();
            for ring in &placement.rings {
                for id in ring {
                    assert!(seen.insert(*id), "id {id} seated twice");
                }
            }
            for id in &placement.horizon {
                assert!(seen.insert(*id), "id {id} double-counted");
            }
            assert_eq!(seen.len(), contexts.len(), "conservation holds despite the corrupt row");
        }
    }
}
