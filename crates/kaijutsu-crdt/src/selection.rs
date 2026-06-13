//! Interval selection — the fork-filter set algebra.
//!
//! Positions address the order-key-sorted, non-deleted snapshot of a block
//! log **at the fork instant**. Every fork shape (full, window, spawn,
//! last-N, notch, bandpass) is an interval filter over that snapshot; this
//! module is the one engine. The earlier rc-rebuilds-vs-prefix-preserve
//! tension is a difference of *intent* expressed as different bases, not
//! competing implementations.
//!
//! The kept-set is
//!
//! ```text
//! kept = (base ∩ ∪includes) \ ∪excludes
//! ```
//!
//! resolved order-free (stacking repeatable flags can never change meaning by
//! position) and one-shot (positions are never stored — what makes positional
//! addressing safe in a multi-writer CRDT log). Output is a normalized
//! [`IntervalSet`] whose runs the splicer (`kaijutsu-kernel::llm::splice`)
//! consumes; the splicer, not this module, owns the *order-dependent* cut
//! hygiene (turn-boundary snapping, tool-pair integrity, archive seams).
//!
//! See `docs/fork-filters.md` (design locked 2026-06-12).

use std::fmt;
use std::ops::Range;

/// A set of half-open `[lo, hi)` intervals over positional block indices,
/// kept **canonical**: runs are sorted, disjoint, non-empty, and merged
/// (adjacent runs `[a, b)` and `[b, c)` collapse to `[a, c)` — they denote the
/// same set of positions). Two `IntervalSet`s are equal iff they cover the
/// same positions, so `PartialEq` is set equality.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IntervalSet {
    /// Canonical runs (sorted, disjoint, merged, all non-empty).
    runs: Vec<Range<usize>>,
}

impl IntervalSet {
    /// The empty set.
    pub fn empty() -> Self {
        Self { runs: Vec::new() }
    }

    /// Everything in `[0, len)`. `len == 0` is the empty set.
    pub fn full(len: usize) -> Self {
        let mut runs = Vec::new();
        if len > 0 {
            runs.push(0..len);
        }
        Self { runs }
    }

    /// Build from arbitrary ranges, normalizing to canonical form: empty and
    /// reversed (`hi <= lo`) ranges are dropped, the rest sorted and merged.
    pub fn from_ranges<I: IntoIterator<Item = Range<usize>>>(iter: I) -> Self {
        let mut runs: Vec<Range<usize>> = iter.into_iter().filter(|r| r.start < r.end).collect();
        runs.sort_by_key(|r| (r.start, r.end));
        let mut merged: Vec<Range<usize>> = Vec::with_capacity(runs.len());
        for r in runs {
            match merged.last_mut() {
                // Overlap OR adjacency (`last.end == r.start`) → extend.
                Some(last) if r.start <= last.end => last.end = last.end.max(r.end),
                _ => merged.push(r),
            }
        }
        Self { runs: merged }
    }

    /// True when the set covers no positions.
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    /// The canonical runs, borrowed.
    pub fn runs(&self) -> &[Range<usize>] {
        &self.runs
    }

    /// Consume into the canonical run vector (what the splicer wants).
    pub fn into_runs(self) -> Vec<Range<usize>> {
        self.runs
    }

    /// Total number of positions covered.
    pub fn count(&self) -> usize {
        self.runs.iter().map(Range::len).sum()
    }

    /// Set union — every position in `self` or `other`.
    pub fn union(&self, other: &Self) -> Self {
        Self::from_ranges(self.runs.iter().cloned().chain(other.runs.iter().cloned()))
    }

    /// Set intersection — positions in both `self` and `other`. Inputs are
    /// canonical (sorted, disjoint), so a single merge walk suffices.
    pub fn intersect(&self, other: &Self) -> Self {
        let mut out = Vec::new();
        let (mut i, mut j) = (0, 0);
        while i < self.runs.len() && j < other.runs.len() {
            let a = &self.runs[i];
            let b = &other.runs[j];
            let lo = a.start.max(b.start);
            let hi = a.end.min(b.end);
            if lo < hi {
                out.push(lo..hi);
            }
            // Advance whichever ends first; the other may still overlap a later run.
            if a.end < b.end {
                i += 1;
            } else {
                j += 1;
            }
        }
        // Already sorted & disjoint by construction, but normalize for safety.
        Self::from_ranges(out)
    }

    /// Set difference — positions in `self` but not in `other`.
    pub fn difference(&self, other: &Self) -> Self {
        let mut out = Vec::new();
        for a in &self.runs {
            // Carve `other`'s runs out of `a`, left to right.
            let mut cursor = a.start;
            for b in &other.runs {
                if b.end <= cursor {
                    continue; // `b` is entirely left of the remaining piece
                }
                if b.start >= a.end {
                    break; // `b` and everything after starts past `a`
                }
                if b.start > cursor {
                    out.push(cursor..b.start.min(a.end));
                }
                cursor = cursor.max(b.end);
                if cursor >= a.end {
                    break;
                }
            }
            if cursor < a.end {
                out.push(cursor..a.end);
            }
        }
        Self::from_ranges(out)
    }

    /// True when every position of `other` is also in `self` (`other ⊆ self`).
    pub fn contains_subset(&self, other: &Self) -> bool {
        other.difference(self).is_empty()
    }

    /// True when the single position `pos` falls within one of the runs. The
    /// positional-membership test a consumer uses while walking an ordered
    /// snapshot ("keep this index?").
    pub fn contains_position(&self, pos: usize) -> bool {
        self.runs.iter().any(|r| r.contains(&pos))
    }
}

/// Why a selection refused. Carries machine-usable data; the CLI layer
/// (`kj fork`'s `resolve_fork_selection`) formats the human message, naming the
/// offending preset/range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionError {
    /// One or more explicitly `--include`d positions did not survive into the
    /// keep-set — a preset's shape or an exclude ate part of an explicit
    /// include, or an include/exclude on the same line contradict. The fork
    /// refuses rather than pick a silent winner. `missing` is the offending
    /// positions (canonical runs).
    IncludeViolation { missing: Vec<Range<usize>> },
}

impl fmt::Display for SelectionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SelectionError::IncludeViolation { missing } => {
                write!(
                    f,
                    "include conflicts with the selection: {} \
                     fall outside the kept set (a preset or exclude removed them). \
                     Drop the preset, adjust the range, or exclude explicitly.",
                    fmt_runs(missing)
                )
            }
        }
    }
}

impl std::error::Error for SelectionError {}

fn fmt_runs(runs: &[Range<usize>]) -> String {
    runs.iter()
        .map(|r| format!("{}:{}", r.start, r.end))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Resolve `kept = (base ∩ ∪includes) \ ∪excludes`, enforcing the include
/// invariant.
///
/// `cli_includes`:
/// - `None` — no `--include` was given; includes default to everything, so
///   the formula is subtract-only and the invariant is vacuous.
/// - `Some(set)` — the union of explicit `--include` ranges. **Every position
///   in it must survive into `kept`**, or this returns
///   [`SelectionError::IncludeViolation`] naming the positions that didn't —
///   no silent winner between a preset's shape, an exclude, and an explicit
///   include.
///
/// `excludes` is the union of all subtractions (preset rows + CLI flags);
/// subtractions union across layers.
pub fn resolve_keep_set(
    base: &IntervalSet,
    cli_includes: Option<&IntervalSet>,
    excludes: &IntervalSet,
) -> Result<IntervalSet, SelectionError> {
    // No `--include` → includes = everything; `base ∩ everything = base`, so
    // intersecting with `base` itself is the subtract-only path.
    let includes = cli_includes.unwrap_or(base);
    let kept = base.intersect(includes).difference(excludes);

    if let Some(inc) = cli_includes
        && !kept.contains_subset(inc)
    {
        return Err(SelectionError::IncludeViolation {
            missing: inc.difference(&kept).into_runs(),
        });
    }
    Ok(kept)
}

/// The `window` preset keep-set: the prefix `[0, marker]` (inclusive of the
/// marked block) unioned with the last `window` blocks. `marker_idx == None`
/// → the whole log (full).
///
/// This is the **single definition** of the window shape, shared by per-turn
/// hydration (the kernel mailbox resolves the marker `BlockId` to its index,
/// then calls here) and the `window` fork preset (slice 3). The union may
/// overlap when the window reaches into the prefix; canonicalization merges
/// it, so a short log returns whole with no gap and no duplicate.
pub fn window_base(len: usize, marker_idx: Option<usize>, window: usize) -> IntervalSet {
    let Some(m) = marker_idx else {
        return IntervalSet::full(len);
    };
    let prefix_end = (m + 1).min(len);
    let tail_start = len.saturating_sub(window);
    IntervalSet::from_ranges([0..prefix_end, tail_start..len])
}

/// Why a range spec failed to parse. Carries the offending text so the CLI can
/// quote it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RangeError {
    /// Not `[lo]:[hi]` — missing or extra colons (block keys with their own
    /// colons stay out of inline ranges; use the dedicated `--exclude <key>`).
    NotARange(String),
    /// An endpoint wasn't `<int> | end | end-<int>` (e.g. a negative, a tick
    /// `t8`, or a label — all reserved for later, not v1).
    BadEndpoint(String),
    /// Resolved to `lo > hi` — a reversed range, almost certainly a mistake.
    Reversed { lo: usize, hi: usize },
}

impl fmt::Display for RangeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RangeError::NotARange(s) => write!(
                f,
                "'{s}' is not a range — expected [lo]:[hi] with a single colon"
            ),
            RangeError::BadEndpoint(s) => write!(
                f,
                "'{s}' is not a valid endpoint — expected an integer, `end`, or `end-N`"
            ),
            RangeError::Reversed { lo, hi } => {
                write!(f, "range start {lo} is past its end {hi}")
            }
        }
    }
}

impl std::error::Error for RangeError {}

/// Resolve one endpoint of the v1 grammar (`<int> | end | end-<int>`) against
/// `len`. Empty (`""`) is the caller's job (it supplies the side's default).
/// `end-N` saturates at 0 (no negatives in v1). The result is NOT yet clamped
/// to `len` — the caller clamps after both endpoints resolve.
fn parse_endpoint(s: &str, len: usize) -> Result<usize, RangeError> {
    if s == "end" {
        return Ok(len);
    }
    if let Some(n) = s.strip_prefix("end-") {
        let n: usize = n.parse().map_err(|_| RangeError::BadEndpoint(s.to_string()))?;
        return Ok(len.saturating_sub(n));
    }
    s.parse::<usize>()
        .map_err(|_| RangeError::BadEndpoint(s.to_string()))
}

/// Parse a v1 range spec — `[endpoint]:[endpoint]`, half-open `[lo, hi)` —
/// resolved against the snapshot length `len`. The grammar:
///
/// ```text
/// range    := [endpoint] ':' [endpoint]
/// endpoint := <int> | end | end-<int>
/// ```
///
/// Missing `lo` defaults to `0`, missing `hi` to `end` (`len`). Both endpoints
/// are clamped to `[0, len]` (you can't address past the end — `0:100` on a
/// 5-block log is `0:5`, and `end` exists precisely so you don't guess the
/// length). `lo == hi` is a valid empty range (e.g. `end-0:` is the empty tail
/// the `window` zero-case relies on); `lo > hi` is a [`RangeError::Reversed`].
/// Reserved endpoint forms (ticks `t8`, labels `bridge`) and negatives are
/// [`RangeError::BadEndpoint`] in v1.
pub fn parse_range(spec: &str, len: usize) -> Result<Range<usize>, RangeError> {
    let (lo_s, hi_s) = spec
        .split_once(':')
        .ok_or_else(|| RangeError::NotARange(spec.to_string()))?;
    // A second colon means three+ parts — not a v1 range.
    if hi_s.contains(':') {
        return Err(RangeError::NotARange(spec.to_string()));
    }

    let lo = if lo_s.is_empty() { 0 } else { parse_endpoint(lo_s, len)? };
    let hi = if hi_s.is_empty() { len } else { parse_endpoint(hi_s, len)? };

    let lo = lo.min(len);
    let hi = hi.min(len);
    if lo > hi {
        return Err(RangeError::Reversed { lo, hi });
    }
    Ok(lo..hi)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(runs: &[Range<usize>]) -> IntervalSet {
        IntervalSet::from_ranges(runs.iter().cloned())
    }

    // ── IntervalSet canonicalization ─────────────────────────────────────

    #[test]
    fn full_and_empty() {
        assert!(IntervalSet::full(0).is_empty());
        assert_eq!(IntervalSet::full(5).runs(), &[0..5]);
        assert!(IntervalSet::empty().is_empty());
    }

    #[test]
    fn from_ranges_sorts_merges_and_drops_degenerate() {
        // unsorted + overlapping + adjacent + empty + reversed
        let s = set(&[3..5, 0..3, 2..2, 5..7, 10..8, 6..6]);
        // 0..3 and 3..5 are adjacent → merge; 5..7 adjacent to that → merge;
        // 2..2 empty dropped; 10..8 reversed dropped.
        assert_eq!(s.runs(), &[0..7]);
    }

    #[test]
    fn from_ranges_keeps_disjoint_gaps() {
        let s = set(&[4..6, 0..2]);
        assert_eq!(s.runs(), &[0..2, 4..6]);
        assert_eq!(s.count(), 4);
    }

    #[test]
    fn equality_is_set_equality_not_representation() {
        // Same positions, different input shapes → equal.
        assert_eq!(set(&[0..3, 3..5]), set(&[0..5]));
        assert_eq!(set(&[0..3, 2..5]), set(&[0..5]));
    }

    // ── union / intersect / difference ───────────────────────────────────

    #[test]
    fn union_merges_and_keeps_gaps() {
        assert_eq!(set(&[0..2]).union(&set(&[4..6])), set(&[0..2, 4..6]));
        assert_eq!(set(&[0..3]).union(&set(&[2..5])), set(&[0..5]));
        assert_eq!(set(&[0..3]).union(&IntervalSet::empty()), set(&[0..3]));
    }

    #[test]
    fn intersect_overlaps_only() {
        assert_eq!(set(&[0..5]).intersect(&set(&[2..8])), set(&[2..5]));
        assert!(set(&[0..2]).intersect(&set(&[4..6])).is_empty());
        // multi-run intersection
        assert_eq!(
            set(&[0..10]).intersect(&set(&[2..4, 6..8])),
            set(&[2..4, 6..8])
        );
        assert_eq!(
            set(&[1..3, 5..9]).intersect(&set(&[2..6, 8..12])),
            set(&[2..3, 5..6, 8..9])
        );
    }

    #[test]
    fn difference_carves_holes() {
        assert_eq!(set(&[0..10]).difference(&set(&[3..5])), set(&[0..3, 5..10]));
        assert!(set(&[0..5]).difference(&set(&[0..5])).is_empty());
        assert_eq!(set(&[0..5]).difference(&set(&[10..12])), set(&[0..5]));
        // multiple holes, including edges
        assert_eq!(
            set(&[0..10]).difference(&set(&[0..2, 4..6, 9..10])),
            set(&[2..4, 6..9])
        );
    }

    #[test]
    fn position_membership() {
        let s = set(&[2..4, 7..9]);
        assert!(!s.contains_position(1));
        assert!(s.contains_position(2));
        assert!(s.contains_position(3));
        assert!(!s.contains_position(4)); // half-open: 4 is excluded
        assert!(s.contains_position(8));
        assert!(!s.contains_position(9));
    }

    #[test]
    fn subset_check() {
        assert!(set(&[0..10]).contains_subset(&set(&[2..4])));
        assert!(set(&[0..10]).contains_subset(&IntervalSet::empty()));
        assert!(!set(&[0..5]).contains_subset(&set(&[3..8])));
        assert!(!set(&[0..3, 7..10]).contains_subset(&set(&[2..8])));
    }

    // ── resolve_keep_set: the kept formula + include invariant ───────────

    #[test]
    fn subtract_only_when_no_includes() {
        let base = IntervalSet::full(10);
        let kept = resolve_keep_set(&base, None, &set(&[4..6])).unwrap();
        assert_eq!(kept, set(&[0..4, 6..10]));
    }

    #[test]
    fn includes_narrow_within_base() {
        let base = IntervalSet::full(10);
        let kept = resolve_keep_set(&base, Some(&set(&[2..5])), &IntervalSet::empty()).unwrap();
        assert_eq!(kept, set(&[2..5]));
    }

    #[test]
    fn include_satisfied_with_exclude_elsewhere() {
        let base = IntervalSet::full(30);
        let kept =
            resolve_keep_set(&base, Some(&set(&[10..20])), &set(&[25..28])).unwrap();
        assert_eq!(kept, set(&[10..20]));
    }

    #[test]
    fn preset_eats_include_is_a_loud_error() {
        // window-shaped base with a notch over [3, 7); an include landing in
        // the notch must fail, not silently vanish.
        let base = set(&[0..3, 7..10]);
        let err = resolve_keep_set(&base, Some(&set(&[4..6])), &IntervalSet::empty())
            .unwrap_err();
        assert_eq!(
            err,
            SelectionError::IncludeViolation { missing: vec![4..6] }
        );
    }

    #[test]
    fn same_line_include_exclude_contradiction_is_an_error() {
        // --include 10:20 --exclude 15:18 → no silent excludes-win.
        let base = IntervalSet::full(30);
        let err = resolve_keep_set(&base, Some(&set(&[10..20])), &set(&[15..18]))
            .unwrap_err();
        assert_eq!(
            err,
            SelectionError::IncludeViolation { missing: vec![15..18] }
        );
    }

    // ── window_base: the shared `window` shape ───────────────────────────

    #[test]
    fn window_base_none_marker_is_full() {
        assert_eq!(window_base(7, None, 3), IntervalSet::full(7));
    }

    #[test]
    fn window_base_overlap_merges() {
        // marker idx 2 (prefix [0,3)), window 4 over len 5 (tail [1,5)) → whole.
        assert_eq!(window_base(5, Some(2), 4), set(&[0..5]));
    }

    #[test]
    fn window_base_zero_window_is_prefix_only() {
        // tail run is empty (`len..len`) → dropped by canonicalization.
        assert_eq!(window_base(6, Some(1), 0), set(&[0..2]));
    }

    #[test]
    fn window_base_disjoint_prefix_and_tail() {
        // The rehydrate scenario: marker idx 1 (prefix [0,2)), window 2 over
        // len 6 (tail [4,6)) → a real gap the splicer will seam.
        assert_eq!(window_base(6, Some(1), 2), set(&[0..2, 4..6]));
    }

    // ── parse_range: the v1 range grammar ────────────────────────────────

    #[test]
    fn parse_range_grammar_examples() {
        // The examples from docs/fork-filters.md, len = 10.
        assert_eq!(parse_range("0:5", 10).unwrap(), 0..5);
        assert_eq!(parse_range(":5", 10).unwrap(), 0..5);
        assert_eq!(parse_range("5:", 10).unwrap(), 5..10);
        assert_eq!(parse_range(":", 10).unwrap(), 0..10);
        assert_eq!(parse_range("0:end", 10).unwrap(), 0..10);
        assert_eq!(parse_range("end-5:", 10).unwrap(), 5..10);
        assert_eq!(parse_range("end-5:end", 10).unwrap(), 5..10);
    }

    #[test]
    fn parse_range_end_minus_saturates_and_empty_tail_is_valid() {
        // end-0: is the empty tail the `window` zero-case relies on.
        assert_eq!(parse_range("end-0:", 6).unwrap(), 6..6);
        // end-N saturates at 0 (no negatives in v1).
        assert_eq!(parse_range("end-100:", 5).unwrap(), 0..5);
    }

    #[test]
    fn parse_range_clamps_to_len() {
        // You can't address past the end; `end` exists so you don't guess.
        assert_eq!(parse_range("0:100", 5).unwrap(), 0..5);
        assert_eq!(parse_range("8:9", 5).unwrap(), 5..5); // both past end → empty
    }

    #[test]
    fn parse_range_reversed_is_an_error() {
        assert_eq!(parse_range("5:3", 10), Err(RangeError::Reversed { lo: 5, hi: 3 }));
        assert_eq!(parse_range("7:4", 10), Err(RangeError::Reversed { lo: 7, hi: 4 }));
    }

    #[test]
    fn parse_range_rejects_non_ranges() {
        // No colon at all.
        assert_eq!(parse_range("5", 10), Err(RangeError::NotARange("5".into())));
        // Block-key shaped (extra colons) stays out of inline ranges.
        assert_eq!(parse_range("a:b:42", 10), Err(RangeError::NotARange("a:b:42".into())));
    }

    #[test]
    fn parse_range_rejects_reserved_and_bad_endpoints() {
        // Reserved-for-later forms parse as bad endpoints in v1.
        assert!(matches!(parse_range("t8:t16", 10), Err(RangeError::BadEndpoint(_)))); // ticks
        assert!(matches!(parse_range("0:bridge", 10), Err(RangeError::BadEndpoint(_)))); // labels
        assert!(matches!(parse_range("-5:", 10), Err(RangeError::BadEndpoint(_)))); // negatives
        assert!(matches!(parse_range("end-:5", 10), Err(RangeError::BadEndpoint(_)))); // malformed end-
        assert!(matches!(parse_range("x:5", 10), Err(RangeError::BadEndpoint(_))));
    }

    #[test]
    fn parsed_ranges_feed_the_set_algebra() {
        // The whole point: parse several specs into an IntervalSet.
        let specs = ["0:2", "end-3:"];
        let parsed: Vec<_> = specs.iter().map(|s| parse_range(s, 10).unwrap()).collect();
        assert_eq!(IntervalSet::from_ranges(parsed), set(&[0..2, 7..10]));
    }
}
