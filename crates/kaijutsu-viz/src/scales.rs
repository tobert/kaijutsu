//! D3-style scales: pure domain→range maps with invertibility.
//!
//! Scales here are `ScaleLinear`, `ScaleTime`, and `ScaleThreshold`. All
//! continuous scales clamp **only when explicitly opted in** via `.clamp(true)`.
//!
//! # Radial-band helper
//!
//! [`RadialBands`] encodes the "history grows denser, not bigger" rule: given N
//! bands and a total radius, each band receives an *equal radial width* regardless
//! of how much time it spans. The innermost band (index 0) maps to the core of
//! the well; the outermost (index N-1) to the rim.


// ─── ScaleLinear ─────────────────────────────────────────────────────────────

/// A continuous linear scale: `domain [d0, d1] → range [r0, r1]`.
///
/// # No-clamp default
///
/// Values outside `[d0, d1]` extrapolate linearly. Enable clamping with
/// [`ScaleLinear::clamp`].
///
/// # Degenerate domain
///
/// When `d0 == d1` the scale is a constant map. `scale(x)` returns `r0` for all
/// `x`; `invert(y)` returns `d0` for all `y`.
#[derive(Debug, Clone, PartialEq)]
pub struct ScaleLinear {
    d0: f64,
    d1: f64,
    r0: f64,
    r1: f64,
    clamp: bool,
}

impl ScaleLinear {
    /// Construct a new linear scale mapping `[d0, d1]` → `[r0, r1]`.
    pub fn new(d0: f64, d1: f64, r0: f64, r1: f64) -> Self {
        Self { d0, d1, r0, r1, clamp: false }
    }

    /// Enable or disable output clamping. Clamping is **off by default**.
    ///
    /// When enabled, `scale` pins its result to `[r0.min(r1), r0.max(r1)]` and
    /// `invert` pins its result to `[d0.min(d1), d0.max(d1)]`.
    #[must_use]
    pub fn clamp(mut self, enabled: bool) -> Self {
        self.clamp = enabled;
        self
    }

    /// Map a domain value to the range.
    pub fn scale(&self, x: f64) -> f64 {
        let dd = self.d1 - self.d0;
        if dd == 0.0 {
            return self.r0;
        }
        let t = (x - self.d0) / dd;
        let y = self.r0 + t * (self.r1 - self.r0);
        if self.clamp {
            clamp_range(y, self.r0, self.r1)
        } else {
            y
        }
    }

    /// Invert a range value back to the domain.
    pub fn invert(&self, y: f64) -> f64 {
        let dr = self.r1 - self.r0;
        if dr == 0.0 {
            return self.d0;
        }
        let t = (y - self.r0) / dr;
        let x = self.d0 + t * (self.d1 - self.d0);
        if self.clamp {
            clamp_range(x, self.d0, self.d1)
        } else {
            x
        }
    }
}

// ─── ScaleTime ───────────────────────────────────────────────────────────────

/// A time scale: `domain [t0, t1]` (Unix milliseconds, `i64`) → `range [r0, r1]`
/// (`f64`).
///
/// Internally this is a thin wrapper around [`ScaleLinear`] with `i64` domain
/// endpoints. Tick/nice generation is out of scope for the current spike.
///
/// # No-clamp default
///
/// Same extrapolation semantics as `ScaleLinear`. Enable clamping with
/// [`ScaleTime::clamp`].
#[derive(Debug, Clone, PartialEq)]
pub struct ScaleTime {
    inner: ScaleLinear,
}

impl ScaleTime {
    /// Construct a new time scale mapping `[t0, t1]` (ms) → `[r0, r1]`.
    ///
    /// # Precision note
    ///
    /// Endpoints are cast to `f64` internally. `f64` has 53 bits of mantissa,
    /// so timestamps beyond ≈ 2^53 ms (year ~285 million) lose sub-millisecond
    /// precision. For realistic Unix-ms ranges (e.g. year 1970–2100) this is
    /// entirely safe.
    pub fn new(t0: i64, t1: i64, r0: f64, r1: f64) -> Self {
        Self { inner: ScaleLinear::new(t0 as f64, t1 as f64, r0, r1) }
    }

    /// Enable or disable output clamping. Clamping is **off by default**.
    #[must_use]
    pub fn clamp(mut self, enabled: bool) -> Self {
        self.inner = self.inner.clamp(enabled);
        self
    }

    /// Map a Unix-millisecond timestamp to the range.
    ///
    /// The timestamp is cast to `f64`; see [`ScaleTime::new`] for the precision note.
    pub fn scale(&self, ms: i64) -> f64 {
        self.inner.scale(ms as f64)
    }

    /// Invert a range value back to a Unix-millisecond timestamp.
    pub fn invert(&self, y: f64) -> i64 {
        self.inner.invert(y).round() as i64
    }
}

// ─── ScaleThreshold ──────────────────────────────────────────────────────────

/// A threshold (quantizing) scale: N-1 sorted thresholds partition the domain
/// into N buckets, each mapped to one range value.
///
/// Matches the contract of D3's `scaleThreshold`:
/// - Thresholds define **left-closed** bucket boundaries: `[−∞, thresholds[0])`,
///   `[thresholds[0], thresholds[1])`, …, `[thresholds[N-2], +∞)`.
/// - `scale(x)` returns `range[i]` where `i` is the index of the first
///   threshold strictly greater than `x` (binary search — O(log N)).
/// - `invert_extent(value)` returns `(lo, hi)` where `lo` is `None` for the
///   first bucket and `hi` is `None` for the last, matching d3's open-ended
///   buckets at the extremes.
///
/// # Type parameters
///
/// `R` is the range value type. It must be `Clone + PartialEq`.
///
/// # Panics
///
/// `new` panics if `thresholds.len() + 1 != range.len()`.
#[derive(Debug, Clone)]
pub struct ScaleThreshold<R> {
    thresholds: Vec<f64>,
    range: Vec<R>,
}

impl<R: Clone + PartialEq> ScaleThreshold<R> {
    /// Construct a threshold scale.
    ///
    /// `thresholds` must be sorted ascending. An unsorted slice violates the
    /// precondition of `partition_point` and produces arbitrary (silently wrong)
    /// bucket assignments — this is checked with a panic at construction time.
    /// `range.len()` must equal `thresholds.len() + 1`.
    pub fn new(thresholds: Vec<f64>, range: Vec<R>) -> Self {
        assert_eq!(
            range.len(),
            thresholds.len() + 1,
            "ScaleThreshold: range.len() must be thresholds.len() + 1 \
             ({} thresholds → {} range values expected, got {})",
            thresholds.len(),
            thresholds.len() + 1,
            range.len()
        );
        assert!(
            thresholds.windows(2).all(|w| w[0] <= w[1]),
            "ScaleThreshold: thresholds must be sorted ascending"
        );
        Self { thresholds, range }
    }

    /// Map a domain value to its range bucket.
    pub fn scale(&self, x: f64) -> &R {
        let idx = self.bucket_index(x);
        &self.range[idx]
    }

    /// Return the `[lo, hi)` domain interval for the bucket that holds `value`.
    ///
    /// Returns `(None, None)` when `value` is not found in the range (no
    /// matching bucket). Otherwise:
    /// - The first bucket has `lo = None` (open on the left: `(−∞, t0)`).
    /// - The last bucket has `hi = None` (open on the right: `[tN-1, +∞)`).
    pub fn invert_extent(&self, value: &R) -> (Option<f64>, Option<f64>) {
        let idx = self.range.iter().position(|r| r == value);
        match idx {
            None => (None, None),
            Some(i) => {
                let lo = if i == 0 { None } else { Some(self.thresholds[i - 1]) };
                let hi = if i >= self.thresholds.len() { None } else { Some(self.thresholds[i]) };
                (lo, hi)
            }
        }
    }

    /// Binary-search the thresholds to find the bucket index for `x`.
    fn bucket_index(&self, x: f64) -> usize {
        // NaN domain values are a caller bug — signal it loudly in debug builds.
        debug_assert!(!x.is_nan(), "ScaleThreshold::scale: NaN domain value");
        // We want the leftmost threshold *strictly greater than* x.
        // `partition_point` returns the first index where the predicate is false.
        self.thresholds.partition_point(|&t| {
            // NaN handling: `t.partial_cmp(&NaN)` is `None` for every threshold,
            // so the predicate is `true` for all thresholds, and `partition_point`
            // returns `thresholds.len()` → the **last** bucket. This matches d3's
            // behaviour where NaN lands in the last (highest) bucket.
            !matches!(t.partial_cmp(&x), Some(std::cmp::Ordering::Greater))
        })
    }
}

// ─── RadialBands ─────────────────────────────────────────────────────────────

/// Equal-width radial annuli for the time-well's idle-age bands.
///
/// Each band receives the same *radial width* regardless of how much time it
/// spans — the "history grows denser, not bigger" rule (`docs/timewell.md`,
/// substrate-notes appendix). The innermost band (index 0) maps to the core; the
/// outermost (index `n_bands - 1`) to the rim.
///
/// Given `(band_index, fraction)` where `fraction ∈ [0.0, 1.0]` is the
/// position within the band (0 = inner edge, 1 = outer edge), [`RadialBands::radius`]
/// returns the corresponding radius.
///
/// # Panics
///
/// `radius` panics if `band_index >= n_bands`.
#[derive(Debug, Clone)]
pub struct RadialBands {
    total_radius: f64,
    n_bands: usize,
}

impl RadialBands {
    /// Construct a radial-band helper with the given total radius and band count.
    pub fn new(total_radius: f64, n_bands: usize) -> Self {
        assert!(n_bands > 0, "RadialBands: n_bands must be > 0");
        Self { total_radius, n_bands }
    }

    /// Width of each band (equal for all bands).
    pub fn band_width(&self) -> f64 {
        self.total_radius / self.n_bands as f64
    }

    /// Map `(band_index, fraction_within_band)` → radius.
    ///
    /// `fraction` is the position within the band: 0.0 = inner edge, 1.0 = outer
    /// edge. Values outside `[0.0, 1.0]` extrapolate (consistent with the crate's
    /// no-silent-clamp stance). Passing a fraction outside this range is a caller
    /// bug and is flagged by `debug_assert!` in debug builds.
    pub fn radius(&self, band_index: usize, fraction: f64) -> f64 {
        assert!(
            band_index < self.n_bands,
            "RadialBands::radius: band_index {} >= n_bands {}",
            band_index,
            self.n_bands
        );
        debug_assert!(
            (0.0..=1.0).contains(&fraction),
            "RadialBands::radius: fraction out of [0,1]: {fraction}"
        );
        let w = self.band_width();
        w * band_index as f64 + w * fraction
    }

    /// Inner-edge radius of `band_index`.
    pub fn inner_radius(&self, band_index: usize) -> f64 {
        self.radius(band_index, 0.0)
    }

    /// Outer-edge radius of `band_index`.
    pub fn outer_radius(&self, band_index: usize) -> f64 {
        self.radius(band_index, 1.0)
    }
}

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Clamp `v` to the interval `[a, b]` or `[b, a]` (handles reversed ranges).
fn clamp_range(v: f64, a: f64, b: f64) -> f64 {
    let lo = a.min(b);
    let hi = a.max(b);
    v.clamp(lo, hi)
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const EPSILON: f64 = 1e-10;

    fn approx_eq(a: f64, b: f64) -> bool {
        (a - b).abs() < EPSILON
    }

    // ── ScaleLinear unit tests ──────────────────────────────────────────────

    #[test]
    fn linear_known_mapping() {
        // [0, 10] → [0, 100]: midpoint should map to 50
        let s = ScaleLinear::new(0.0, 10.0, 0.0, 100.0);
        assert!(approx_eq(s.scale(5.0), 50.0), "scale(5) should be 50");
        assert!(approx_eq(s.invert(50.0), 5.0), "invert(50) should be 5");
    }

    #[test]
    fn linear_endpoints() {
        let s = ScaleLinear::new(0.0, 10.0, 0.0, 100.0);
        assert!(approx_eq(s.scale(0.0), 0.0));
        assert!(approx_eq(s.scale(10.0), 100.0));
        assert!(approx_eq(s.invert(0.0), 0.0));
        assert!(approx_eq(s.invert(100.0), 10.0));
    }

    #[test]
    fn linear_negative_domain() {
        let s = ScaleLinear::new(-10.0, 10.0, 0.0, 200.0);
        // x=0 is midpoint of [-10, 10], should map to 100
        assert!(approx_eq(s.scale(0.0), 100.0));
        assert!(approx_eq(s.invert(100.0), 0.0));
    }

    #[test]
    fn linear_reversed_range() {
        // Domain [0,1] → range [100, 0]: scale and invert should both work
        let s = ScaleLinear::new(0.0, 1.0, 100.0, 0.0);
        assert!(approx_eq(s.scale(0.5), 50.0));
        assert!(approx_eq(s.invert(50.0), 0.5));
    }

    #[test]
    fn linear_degenerate_domain_returns_r0() {
        // d0 == d1: constant map to r0
        let s = ScaleLinear::new(5.0, 5.0, 10.0, 20.0);
        assert!(approx_eq(s.scale(0.0), 10.0));
        assert!(approx_eq(s.scale(5.0), 10.0));
        assert!(approx_eq(s.scale(99.0), 10.0));
        // invert returns d0
        assert!(approx_eq(s.invert(0.0), 5.0));
    }

    // S3: degenerate range (r0 == r1) — invert returns d0 for all y.
    #[test]
    fn linear_degenerate_range_invert_returns_d0() {
        let s = ScaleLinear::new(0.0, 10.0, 5.0, 5.0);
        // scale is constant: all x map to r0=5.0
        assert!(approx_eq(s.scale(0.0), 5.0));
        assert!(approx_eq(s.scale(5.0), 5.0));
        assert!(approx_eq(s.scale(10.0), 5.0));
        assert!(approx_eq(s.scale(-99.0), 5.0));
        // invert is degenerate: all y return d0=0.0
        assert!(approx_eq(s.invert(0.0), 0.0));
        assert!(approx_eq(s.invert(5.0), 0.0));
        assert!(approx_eq(s.invert(100.0), 0.0));
        assert!(approx_eq(s.invert(-50.0), 0.0));
    }

    #[test]
    fn linear_extrapolates_unclamped() {
        // x outside [0, 1] should extrapolate, not clamp
        let s = ScaleLinear::new(0.0, 1.0, 0.0, 100.0);
        assert!(approx_eq(s.scale(2.0), 200.0), "should extrapolate to 200, not clamp to 100");
        assert!(approx_eq(s.scale(-1.0), -100.0), "should extrapolate to -100, not clamp to 0");
    }

    #[test]
    fn linear_clamped_pins_to_range() {
        let s = ScaleLinear::new(0.0, 1.0, 0.0, 100.0).clamp(true);
        assert!(approx_eq(s.scale(2.0), 100.0), "clamped: should pin at 100");
        assert!(approx_eq(s.scale(-1.0), 0.0), "clamped: should pin at 0");
    }

    #[test]
    fn linear_clamped_invert_pins_to_domain() {
        let s = ScaleLinear::new(0.0, 1.0, 0.0, 100.0).clamp(true);
        assert!(approx_eq(s.invert(200.0), 1.0), "clamped invert: should pin at d1");
        assert!(approx_eq(s.invert(-50.0), 0.0), "clamped invert: should pin at d0");
    }

    // ── ScaleTime unit tests ────────────────────────────────────────────────

    #[test]
    fn time_known_mapping() {
        // 1000ms span → [0.0, 1.0]: midpoint (500ms) should map to 0.5
        let s = ScaleTime::new(0, 1000, 0.0, 1.0);
        assert!(approx_eq(s.scale(500), 0.5));
        assert_eq!(s.invert(0.5), 500);
    }

    #[test]
    fn time_endpoints() {
        let s = ScaleTime::new(1_000_000_000_000, 2_000_000_000_000, 0.0, 100.0);
        assert!(approx_eq(s.scale(1_000_000_000_000), 0.0));
        assert!(approx_eq(s.scale(2_000_000_000_000), 100.0));
        assert_eq!(s.invert(0.0), 1_000_000_000_000);
        assert_eq!(s.invert(100.0), 2_000_000_000_000);
    }

    #[test]
    fn time_degenerate_domain() {
        let s = ScaleTime::new(42, 42, 0.0, 1.0);
        assert!(approx_eq(s.scale(42), 0.0));
        assert_eq!(s.invert(0.5), 42);
    }

    #[test]
    fn time_extrapolates_unclamped() {
        let s = ScaleTime::new(0, 1000, 0.0, 1.0);
        assert!(s.scale(2000) > 1.0, "should extrapolate past r1");
        assert!(s.scale(-500) < 0.0, "should extrapolate past r0");
    }

    #[test]
    fn time_clamped() {
        let s = ScaleTime::new(0, 1000, 0.0, 1.0).clamp(true);
        assert!(approx_eq(s.scale(2000), 1.0));
        assert!(approx_eq(s.scale(-500), 0.0));
    }

    // ── ScaleThreshold unit tests ───────────────────────────────────────────

    #[test]
    fn threshold_three_buckets() {
        // thresholds [10, 20] → buckets: x<10 → "low", 10≤x<20 → "mid", x≥20 → "high"
        let s = ScaleThreshold::new(vec![10.0, 20.0], vec!["low", "mid", "high"]);
        assert_eq!(*s.scale(5.0), "low");
        assert_eq!(*s.scale(10.0), "mid");
        assert_eq!(*s.scale(15.0), "mid");
        assert_eq!(*s.scale(20.0), "high");
        assert_eq!(*s.scale(100.0), "high");
    }

    #[test]
    fn threshold_boundary_is_left_closed() {
        // Boundary value must land in the *upper* bucket (left-closed intervals)
        let s = ScaleThreshold::new(vec![0.5], vec!["below", "above"]);
        assert_eq!(*s.scale(0.5), "above", "threshold boundary is the start of the next bucket");
        assert_eq!(*s.scale(0.4999), "below");
    }

    #[test]
    fn threshold_single_threshold() {
        let s = ScaleThreshold::new(vec![0.0], vec!["neg", "pos"]);
        assert_eq!(*s.scale(-1.0), "neg");
        assert_eq!(*s.scale(0.0), "pos"); // 0.0 is ≥ threshold → upper bucket
        assert_eq!(*s.scale(1.0), "pos");
    }

    #[test]
    fn threshold_invert_extent_open_first_bucket() {
        let s = ScaleThreshold::new(vec![10.0, 20.0], vec!["low", "mid", "high"]);
        // First bucket: lo is unbounded
        let (lo, hi) = s.invert_extent(&"low");
        assert!(lo.is_none(), "first bucket lower bound is open (None)");
        assert!(approx_eq(hi.unwrap(), 10.0));
    }

    #[test]
    fn threshold_invert_extent_middle_bucket() {
        let s = ScaleThreshold::new(vec![10.0, 20.0], vec!["low", "mid", "high"]);
        let (lo, hi) = s.invert_extent(&"mid");
        assert!(approx_eq(lo.unwrap(), 10.0));
        assert!(approx_eq(hi.unwrap(), 20.0));
    }

    #[test]
    fn threshold_invert_extent_open_last_bucket() {
        let s = ScaleThreshold::new(vec![10.0, 20.0], vec!["low", "mid", "high"]);
        // Last bucket: hi is unbounded
        let (lo, hi) = s.invert_extent(&"high");
        assert!(approx_eq(lo.unwrap(), 20.0));
        assert!(hi.is_none(), "last bucket upper bound is open (None)");
    }

    #[test]
    fn threshold_invert_extent_not_found() {
        let s = ScaleThreshold::new(vec![10.0], vec!["a", "b"]);
        let (lo, hi) = s.invert_extent(&"z");
        assert!(lo.is_none());
        assert!(hi.is_none());
    }

    #[test]
    #[should_panic(expected = "ScaleThreshold: range.len() must be thresholds.len() + 1")]
    fn threshold_panics_on_bad_lengths() {
        let _ = ScaleThreshold::new(vec![1.0, 2.0], vec!["only_one"]);
    }

    // B1: unsorted thresholds must panic at construction.
    #[test]
    #[should_panic(expected = "ScaleThreshold: thresholds must be sorted ascending")]
    fn threshold_panics_on_unsorted_thresholds() {
        let _ = ScaleThreshold::new(vec![20.0, 10.0], vec!["a", "b", "c"]);
    }

    // B2: NaN lands in the last bucket (matches d3 behaviour). Only verifiable
    // in release builds because the debug_assert fires before the bucket lookup.
    #[test]
    #[cfg(not(debug_assertions))]
    fn threshold_nan_lands_in_last_bucket() {
        let s = ScaleThreshold::new(vec![10.0, 20.0], vec!["low", "mid", "high"]);
        assert_eq!(*s.scale(f64::NAN), "high", "NaN should fall into the last bucket");
    }

    // ── RadialBands unit tests ──────────────────────────────────────────────

    #[test]
    fn radial_bands_equal_width() {
        let rb = RadialBands::new(300.0, 3);
        // Each band is 100px wide
        assert!(approx_eq(rb.band_width(), 100.0));
    }

    #[test]
    fn radial_bands_edges() {
        let rb = RadialBands::new(300.0, 3);
        // Band 0 (core): 0..100
        assert!(approx_eq(rb.inner_radius(0), 0.0));
        assert!(approx_eq(rb.outer_radius(0), 100.0));
        // Band 1 (mid): 100..200
        assert!(approx_eq(rb.inner_radius(1), 100.0));
        assert!(approx_eq(rb.outer_radius(1), 200.0));
        // Band 2 (rim): 200..300
        assert!(approx_eq(rb.inner_radius(2), 200.0));
        assert!(approx_eq(rb.outer_radius(2), 300.0));
    }

    #[test]
    fn radial_bands_midpoint() {
        let rb = RadialBands::new(300.0, 3);
        // Midpoint of band 1 should be at radius 150
        assert!(approx_eq(rb.radius(1, 0.5), 150.0));
    }

    // S1: fraction outside [0,1] now extrapolates (no silent clamp) in release
    // builds, and fires debug_assert in debug builds.
    #[test]
    #[cfg(not(debug_assertions))]
    fn radial_bands_fraction_extrapolates_unclamped() {
        // Fraction outside [0,1] should extrapolate linearly, not clamp.
        let rb = RadialBands::new(300.0, 3);
        // Band 0 width = 100; fraction=1.5 → 0 + 100*1.5 = 150 (not clamped to 100)
        assert!(approx_eq(rb.radius(0, 1.5), 150.0), "should extrapolate to 150, not clamp to 100");
        // fraction=-0.5 → 0 + 100*(-0.5) = -50 (not clamped to 0)
        assert!(approx_eq(rb.radius(0, -0.5), -50.0), "should extrapolate to -50, not clamp to 0");
    }

    // In debug builds, fraction out of range panics.
    #[test]
    #[cfg(debug_assertions)]
    #[should_panic(expected = "RadialBands::radius: fraction out of [0,1]")]
    fn radial_bands_fraction_out_of_range_panics_debug() {
        let rb = RadialBands::new(300.0, 3);
        let _ = rb.radius(0, 1.5);
    }

    #[test]
    #[should_panic(expected = "RadialBands::radius: band_index 3 >= n_bands 3")]
    fn radial_bands_out_of_bounds_panics() {
        let rb = RadialBands::new(300.0, 3);
        let _ = rb.radius(3, 0.5); // 3 is out of bounds for n_bands=3
    }

    #[test]
    fn radial_bands_three_bands_cover_full_radius() {
        let total = 450.0;
        let rb = RadialBands::new(total, 3);
        // Outer edge of last band must reach the total radius exactly
        assert!(approx_eq(rb.outer_radius(2), total));
    }

    // ── Property tests (proptest) ───────────────────────────────────────────

    #[cfg(test)]
    mod props {
        use super::*;
        use proptest::prelude::*;

        proptest! {
            // ScaleLinear: invert(scale(x)) ≈ x for non-degenerate domains.
            //
            // Floating-point round-trip precision is bounded by the ratio of domain
            // width to range width (the "compression ratio"). When the scale compresses
            // a large domain into a tiny range, the inversion magnifies the rounding
            // error. We constrain the strategy so that the compression ratio stays
            // below ~1000x, which keeps the relative error inside 1e-6.
            #[test]
            fn linear_invert_roundtrip(
                d0 in -1e6_f64..1e6_f64,
                width in 1.0_f64..1e4_f64,
                r0 in -1e6_f64..1e6_f64,
                // Range width is at least 10% of domain width to limit compression ratio
                rwidth_factor in 0.1_f64..10.0_f64,
                x in -2e6_f64..2e6_f64,
            ) {
                let d1 = d0 + width;
                let r1 = r0 + rwidth_factor * width;
                let s = ScaleLinear::new(d0, d1, r0, r1);
                let recovered = s.invert(s.scale(x));
                // Relative tolerance w.r.t. the domain width; absolute floor for small x
                let tol = (x.abs() * 1e-9).max(1e-6);
                prop_assert!(
                    (recovered - x).abs() < tol,
                    "invert(scale({x})) = {recovered} (expected ≈ {x}, tol={tol})"
                );
            }

            // ScaleLinear (clamped): invert(scale(x)) recovers x when x is in-domain.
            // Same compression-ratio constraint as the unclamped variant.
            #[test]
            fn linear_clamped_invert_roundtrip_in_domain(
                d0 in -1e6_f64..1e6_f64,
                width in 1.0_f64..1e4_f64,
                r0 in -1e6_f64..1e6_f64,
                rwidth_factor in 0.1_f64..10.0_f64,
                frac in 0.0_f64..=1.0_f64,
            ) {
                let d1 = d0 + width;
                let r1 = r0 + rwidth_factor * width;
                let x = d0 + frac * width;
                let s = ScaleLinear::new(d0, d1, r0, r1).clamp(true);
                let recovered = s.invert(s.scale(x));
                let tol = (x.abs() * 1e-9).max(1e-6);
                prop_assert!(
                    (recovered - x).abs() < tol,
                    "clamped invert(scale({x})) = {recovered} (expected ≈ {x}, tol={tol})"
                );
            }

            // ScaleTime: invert(scale(ms)) ≈ ms for non-degenerate domains
            // (uses i64 domain; round-trip has ±1ms tolerance from round())
            #[test]
            fn time_invert_roundtrip(
                t0 in 0_i64..1_000_000_000_000_i64,
                span in 1_i64..1_000_000_000_i64,
                frac in 0.0_f64..=1.0_f64,
            ) {
                let t1 = t0 + span;
                let ms = t0 + (frac * span as f64) as i64;
                let s = ScaleTime::new(t0, t1, 0.0, 1.0);
                let recovered = s.invert(s.scale(ms));
                prop_assert!(
                    (recovered - ms).abs() <= 1,
                    "time invert(scale({ms})) = {recovered} (expected ≈ {ms})"
                );
            }

            // ScaleThreshold: scale returns a value from the range
            #[test]
            fn threshold_scale_is_in_range(
                t0 in -1e6_f64..1e6_f64,
                gap in 0.1_f64..1e4_f64,
                x in -2e6_f64..2e6_f64,
            ) {
                let t1 = t0 + gap;
                let s = ScaleThreshold::new(vec![t0, t1], vec![0u32, 1u32, 2u32]);
                let v = *s.scale(x);
                prop_assert!(v <= 2, "scale result must be in [0,2], got {v}");
            }

            // S2a: invert_extent(scale(x)) returns an interval that contains x.
            // We test with a non-NaN x well away from boundaries to keep this clean.
            #[test]
            fn threshold_invert_extent_contains_scaled_value(
                t0 in -1e5_f64..1e5_f64,
                gap in 1.0_f64..1e4_f64,
                // x strictly inside one of the three buckets to avoid boundary edge-cases
                frac in 0.05_f64..0.45_f64,
            ) {
                let t1 = t0 + gap;
                let s = ScaleThreshold::new(
                    vec![t0, t1],
                    vec![0u32, 1u32, 2u32],
                );
                // Pick x strictly inside the middle bucket [t0, t1)
                let x = t0 + frac * gap;
                let bucket = s.scale(x);
                let (lo, hi) = s.invert_extent(bucket);
                // lo ≤ x (or unbounded on left)
                if let Some(lo_val) = lo {
                    prop_assert!(lo_val <= x, "lo ({lo_val}) > x ({x})");
                }
                // x < hi (or unbounded on right, which only happens for last bucket)
                if let Some(hi_val) = hi {
                    prop_assert!(x < hi_val, "x ({x}) >= hi ({hi_val})");
                }
            }

            // S2b: boundary correctness — scale(t_i) == range[i+1] and
            // scale(t_i - tiny) == range[i] for each threshold.
            #[test]
            fn threshold_boundary_correctness(
                t0 in -1e5_f64..1e5_f64,
                gap in 1.0_f64..1e4_f64,
            ) {
                let t1 = t0 + gap;
                let s = ScaleThreshold::new(
                    vec![t0, t1],
                    vec![0u32, 1u32, 2u32],
                );
                // t0 is the first threshold: scale(t0) should land in bucket index 1
                prop_assert_eq!(*s.scale(t0), 1u32,
                    "scale(t0) should be range[1]=1, t0={}", t0);
                // Just below t0: bucket 0
                let below_t0 = t0 - 1e-6 * gap.max(1.0);
                prop_assert_eq!(*s.scale(below_t0), 0u32,
                    "scale(below t0) should be range[0]=0, below_t0={}", below_t0);
                // t1 is the second threshold: scale(t1) should land in bucket index 2
                prop_assert_eq!(*s.scale(t1), 2u32,
                    "scale(t1) should be range[2]=2, t1={}", t1);
                // Just below t1: bucket 1
                let below_t1 = t1 - 1e-6 * gap.max(1.0);
                prop_assert_eq!(*s.scale(below_t1), 1u32,
                    "scale(below t1) should be range[1]=1, below_t1={}", below_t1);
            }

            // RadialBands: radius increases monotonically with band_index and fraction
            #[test]
            fn radial_bands_monotonic(
                total in 1.0_f64..10000.0_f64,
                n in 1_usize..10_usize,
                frac in 0.0_f64..=1.0_f64,
            ) {
                let rb = RadialBands::new(total, n);
                // Each band's inner edge is >= previous band's inner edge
                for i in 1..n {
                    prop_assert!(
                        rb.inner_radius(i) >= rb.inner_radius(i - 1),
                        "band {} inner ({}) < band {} inner ({})",
                        i, rb.inner_radius(i), i - 1, rb.inner_radius(i - 1)
                    );
                }
                // Fraction increase yields radius increase within a band
                if n >= 1 {
                    let r_inner = rb.radius(0, 0.0);
                    let r_at_frac = rb.radius(0, frac);
                    prop_assert!(r_at_frac >= r_inner);
                }
            }

            // N4: outer_radius(n-1) ≈ total_radius under varied total and n.
            #[test]
            fn radial_bands_last_band_reaches_total_radius(
                total in 1.0_f64..10000.0_f64,
                n in 1_usize..20_usize,
            ) {
                let rb = RadialBands::new(total, n);
                let outer = rb.outer_radius(n - 1);
                prop_assert!(
                    (outer - total).abs() < 1e-9,
                    "outer_radius({}) = {outer}, expected {total}",
                    n - 1,
                );
            }
        }
    }
}
