//! Idle-age band classification for the time-well context browser.
//!
//! # Architecture
//!
//! **[`assign_idle_band`]** classifies each context into one of four idle-age
//! [`Band`]s. Unlike the superseded `concluded_at`-keyed scheme, each context's
//! band is a **pure per-context derivation** of `now − last_activity_at` (plus
//! two overrides — see below) — it needs no cross-context window, no N-most-
//! recent set, nothing but its own fields and the current instant. A context
//! decays toward the horizon by idling; nothing has to expire it.
//!
//! This module previously also carried `CompactingBandLayout`, a pure polar
//! layout engine built on `RadialBands` — deleted 2026-07-03 (no consumer since
//! the 7.8 spiral superseded the ring layout; see `docs/timewell.md`, "Where we
//! are" / "Dead code"). The `concluded_at`-keyed `Band`/`assign_band` (Stage 0)
//! is superseded by idle-age bands here (Stage 1, "kernel truth: activity
//! recency" — see `docs/timewell.md`). `RadialBands` (`crate::scales`) stays for
//! now, unused by this module (the terracing in `kaijutsu-app`'s `card.rs`
//! divides its own radius/depth envelope directly rather than going through it).
//!
//! # Fail-loud stance
//!
//! No silent clamping, no silent fallbacks. The two overrides (running, and the
//! conclude-demotes rule) are explicit, ordered, and documented at
//! [`assign_idle_band`] — not implicit tie-breaks.

// ─── Band ────────────────────────────────────────────────────────────────────

/// Idle-age band — how long it has been since a context last saw activity.
///
/// `HotNow` is the mouth of the well (shallowest, largest); `Horizon` is the
/// throat (deepest, smallest). Declared in mouth→throat order so the derived
/// [`Ord`] (used nowhere load-bearing today, kept for free) agrees with
/// [`Band::index`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Band {
    /// Idle < [`HOT_NOW_MILLIS`], or `running`, and not demoted by conclude.
    /// Mouth (index 0).
    HotNow,
    /// Idle < [`THIS_WEEK_MILLIS`] (or demoted here from `HotNow` by conclude).
    ThisWeek,
    /// Idle < [`THIRTY_DAYS_MILLIS`].
    ThirtyDays,
    /// Idle ≥ [`THIRTY_DAYS_MILLIS`]. Throat (index 3).
    Horizon,
}

impl Band {
    /// Numeric band index, mouth→throat: `HotNow` = 0 (shallowest) …
    /// `Horizon` = 3 (deepest). The single source `card.rs`'s terracing and
    /// slot-order code key off — see `docs/timewell.md`, "The bowl, revisited".
    pub fn index(self) -> usize {
        match self {
            Band::HotNow => 0,
            Band::ThisWeek => 1,
            Band::ThirtyDays => 2,
            Band::Horizon => 3,
        }
    }
}

/// All four bands, mouth→throat. The single source for "walk every band in
/// order" call sites (`card.rs`'s `band_orders`/`spiral_order`/terracing).
pub const ALL_BANDS: [Band; 4] = [Band::HotNow, Band::ThisWeek, Band::ThirtyDays, Band::Horizon];

// ─── Boundary constants (Amy-tunable) ──────────────────────────────────────
//
// **Placeholders.** These are the only three numbers that define the idle-age
// ladder; Amy tunes them live once the terraced vortex is on screen. Named and
// single-sourced here so nothing else hardcodes a millisecond count.

/// Idle age below which a context is `HotNow` (~1 day).
pub const HOT_NOW_MILLIS: i64 = 24 * 60 * 60 * 1000;
/// Idle age below which a context is `ThisWeek` (~7 days).
pub const THIS_WEEK_MILLIS: i64 = 7 * 24 * 60 * 60 * 1000;
/// Idle age below which a context is `ThirtyDays` (~30 days). At or beyond
/// this age (and not otherwise overridden) a context is `Horizon`.
pub const THIRTY_DAYS_MILLIS: i64 = 30 * 24 * 60 * 60 * 1000;

// ─── assign_idle_band ───────────────────────────────────────────────────────

/// Context descriptor used by [`assign_idle_band`].
///
/// `Id` only needs `Clone + Debug` (via the derives below) so it can appear in
/// diagnostics; unlike the old `assign_band`, band assignment here never
/// compares one context against another, so no `Ord` bound is needed on `Id`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextLifecycle<Id> {
    /// Stable context identifier.
    pub id: Id,
    /// Unix-millisecond creation time (informational; not used for band
    /// assignment — `last_activity_at` is expected to already coalesce to this
    /// when the kernel has no activity timestamp; see `card.rs::assign_bands`).
    pub created_at: i64,
    /// Unix-millisecond conclusion time, or `None` if the context is still
    /// open. Drives the demote-past-`HotNow` rule below.
    pub concluded_at: Option<i64>,
    /// Unix-millisecond timestamp of the most recent block activity. The sole
    /// input to age classification: `idle = now - last_activity_at`.
    pub last_activity_at: i64,
    /// Whether the context's live status is `Running` right now. Forces
    /// `HotNow` regardless of idle age (unless demoted by conclude — see
    /// [`assign_idle_band`]).
    pub running: bool,
}

/// Assign a [`Band`] to each context in `contexts`, purely from `now` and each
/// context's own fields (no cross-context window).
///
/// Rules, in order:
/// 1. **Age classify**: `idle = now - last_activity_at` buckets into
///    `HotNow` / `ThisWeek` / `ThirtyDays` / `Horizon` via the boundary
///    constants above (strict `<`, so a context exactly at a boundary falls
///    into the *next* colder band).
/// 2. **Running override**: if `running`, the tentative band becomes `HotNow`
///    regardless of idle age.
/// 3. **Conclude-demotes**: if `concluded_at.is_some()` and the tentative band
///    (after the running override) is `HotNow`, force it to `ThisWeek`. An
///    explicit `conclude` always demotes past the mouth immediately, even for
///    a context that is technically still "running" or freshly active —
///    concluding is the mux-exit act and wins over both other rules.
///
/// # Output order
///
/// The output `Vec` is in the same order as `contexts`.
pub fn assign_idle_band<Id>(contexts: &[ContextLifecycle<Id>], now: i64) -> Vec<Band> {
    contexts
        .iter()
        .map(|c| {
            let idle = now - c.last_activity_at;
            let mut band = if c.running {
                Band::HotNow
            } else {
                classify_idle(idle)
            };
            if c.concluded_at.is_some() && band == Band::HotNow {
                band = Band::ThisWeek;
            }
            band
        })
        .collect()
}

/// Bucket a raw idle duration (millis) into a [`Band`] with no overrides.
fn classify_idle(idle_millis: i64) -> Band {
    if idle_millis < HOT_NOW_MILLIS {
        Band::HotNow
    } else if idle_millis < THIS_WEEK_MILLIS {
        Band::ThisWeek
    } else if idle_millis < THIRTY_DAYS_MILLIS {
        Band::ThirtyDays
    } else {
        Band::Horizon
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(id: u32, now: i64, idle_millis: i64, concluded: bool, running: bool) -> ContextLifecycle<u32> {
        ContextLifecycle {
            id,
            created_at: 0,
            concluded_at: if concluded { Some(now - idle_millis / 2) } else { None },
            last_activity_at: now - idle_millis,
            running,
        }
    }

    const DAY: i64 = 24 * 60 * 60 * 1000;
    const NOW: i64 = 1_000_000_000_000; // arbitrary fixed instant

    // ── Band::index ─────────────────────────────────────────────────────────

    #[test]
    fn band_index_is_mouth_to_throat() {
        assert_eq!(Band::HotNow.index(), 0, "HotNow is the mouth");
        assert_eq!(Band::ThisWeek.index(), 1);
        assert_eq!(Band::ThirtyDays.index(), 2);
        assert_eq!(Band::Horizon.index(), 3, "Horizon is the throat");
    }

    #[test]
    fn all_bands_is_mouth_to_throat_order() {
        assert_eq!(
            ALL_BANDS,
            [Band::HotNow, Band::ThisWeek, Band::ThirtyDays, Band::Horizon]
        );
    }

    // ── assign_idle_band: pure age classification ───────────────────────────

    #[test]
    fn idle_under_a_day_is_hot_now() {
        let c = vec![ctx(1, NOW, DAY / 2, false, false)];
        assert_eq!(assign_idle_band(&c, NOW)[0], Band::HotNow, "12h idle -> HotNow");
    }

    #[test]
    fn idle_three_days_is_this_week() {
        let c = vec![ctx(1, NOW, 3 * DAY, false, false)];
        assert_eq!(assign_idle_band(&c, NOW)[0], Band::ThisWeek, "3d idle -> ThisWeek");
    }

    #[test]
    fn idle_fifteen_days_is_thirty_days() {
        let c = vec![ctx(1, NOW, 15 * DAY, false, false)];
        assert_eq!(assign_idle_band(&c, NOW)[0], Band::ThirtyDays, "15d idle -> ThirtyDays");
    }

    #[test]
    fn idle_forty_five_days_is_horizon() {
        let c = vec![ctx(1, NOW, 45 * DAY, false, false)];
        assert_eq!(assign_idle_band(&c, NOW)[0], Band::Horizon, "45d idle -> Horizon");
    }

    #[test]
    fn boundary_ties_fall_to_the_colder_band() {
        // Exactly at each boundary, strict `<` means the colder band wins.
        let c = vec![ctx(1, NOW, HOT_NOW_MILLIS, false, false)];
        assert_eq!(assign_idle_band(&c, NOW)[0], Band::ThisWeek, "exactly 1 day -> ThisWeek");
        let c = vec![ctx(1, NOW, THIS_WEEK_MILLIS, false, false)];
        assert_eq!(assign_idle_band(&c, NOW)[0], Band::ThirtyDays, "exactly 7 days -> ThirtyDays");
        let c = vec![ctx(1, NOW, THIRTY_DAYS_MILLIS, false, false)];
        assert_eq!(assign_idle_band(&c, NOW)[0], Band::Horizon, "exactly 30 days -> Horizon");
    }

    // ── assign_idle_band: overrides ──────────────────────────────────────────

    #[test]
    fn concluded_but_recent_is_demoted_out_of_hot_now() {
        // Idle 1 hour (well within HotNow) but concluded -> demoted to ThisWeek.
        let c = vec![ctx(1, NOW, 60 * 60 * 1000, true, false)];
        assert_eq!(
            assign_idle_band(&c, NOW)[0],
            Band::ThisWeek,
            "conclude demotes past HotNow regardless of recency"
        );
    }

    #[test]
    fn concluded_and_already_colder_than_hot_now_is_unaffected() {
        // Already ThirtyDays; conclude only demotes out of HotNow, so this stays put.
        let c = vec![ctx(1, NOW, 15 * DAY, true, false)];
        assert_eq!(
            assign_idle_band(&c, NOW)[0],
            Band::ThirtyDays,
            "demote rule only fires when the tentative band is HotNow"
        );
    }

    #[test]
    fn running_forces_hot_now_regardless_of_idle_age() {
        let c = vec![ctx(1, NOW, 60 * DAY, false, true)];
        assert_eq!(
            assign_idle_band(&c, NOW)[0],
            Band::HotNow,
            "running overrides idle age"
        );
    }

    #[test]
    fn running_and_concluded_still_demotes() {
        // Pathological (a concluded context shouldn't really be running), but the
        // demote rule is defined to win over the running override — conclude is
        // the explicit mux-exit act.
        let c = vec![ctx(1, NOW, 60 * DAY, true, true)];
        assert_eq!(
            assign_idle_band(&c, NOW)[0],
            Band::ThisWeek,
            "conclude wins over the running override"
        );
    }

    #[test]
    fn open_and_active_and_not_running_is_hot_now() {
        let c = vec![ctx(1, NOW, 0, false, false)];
        assert_eq!(assign_idle_band(&c, NOW)[0], Band::HotNow);
    }

    #[test]
    fn output_order_matches_input_order() {
        let c = vec![
            ctx(1, NOW, 45 * DAY, false, false), // Horizon
            ctx(2, NOW, 0, false, false),        // HotNow
            ctx(3, NOW, 3 * DAY, false, false),  // ThisWeek
        ];
        let bands = assign_idle_band(&c, NOW);
        assert_eq!(bands, vec![Band::Horizon, Band::HotNow, Band::ThisWeek]);
    }

    // ── Property tests ──────────────────────────────────────────────────────

    #[cfg(test)]
    mod props {
        use super::*;
        use proptest::prelude::*;

        // PROP-IDLE-BAND-MONOTONIC: holding running=false, concluded=None, band
        // never gets *warmer* as idle age increases.
        proptest! {
            #[test]
            fn prop_band_is_monotonic_in_idle_age(
                idle_a in 0i64..(200 * 24 * 60 * 60 * 1000),
                extra in 0i64..(200 * 24 * 60 * 60 * 1000),
            ) {
                let idle_b = idle_a + extra;
                let a = ContextLifecycle { id: 1u32, created_at: 0, concluded_at: None, last_activity_at: NOW - idle_a, running: false };
                let b = ContextLifecycle { id: 2u32, created_at: 0, concluded_at: None, last_activity_at: NOW - idle_b, running: false };
                let band_a = assign_idle_band(&[a], NOW)[0];
                let band_b = assign_idle_band(&[b], NOW)[0];
                // Band's derived Ord is mouth(0)->throat(3), so "at least as cold" is >=.
                prop_assert!(band_b >= band_a, "older idle age must not be a warmer band");
            }
        }
    }
}
