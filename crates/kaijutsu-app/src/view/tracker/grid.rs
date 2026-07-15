//! Pure pattern-grid math for the tracker station's E face — no Bevy types,
//! unit-tested first (`docs/tracks.md`: tracks are independent clock
//! domains, so this module's whole job is turning one track's tempo/phrase
//! numbers plus a phasor position into row geometry, with nothing else in
//! the app touching it).
//!
//! Five pieces, mirroring the classic-tracker face `tracker/mod.rs` builds
//! on top of this:
//! - [`column_layout`] — where each track's column sits across the face.
//! - [`tempo_label`] — the header plate's BPM readout.
//! - [`row_count_for`] — how many row entities a column needs (a multiple
//!   of the phrase length, so wrapping never breaks phrase identity).
//! - [`row_offset`] — the scrolling row math: given the phasor's current
//!   beat position, where row `j` sits relative to the fixed playhead.
//! - [`is_phrase_row`] — which rows get the phrase-boundary emphasis.

/// One column's horizontal placement on the face (local X, world units once
/// placed through [`super::STATION_E_PLACEMENT`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ColumnSlot {
    /// Local-X center of the column.
    pub x_center: f32,
    /// Column width (already clamped — never wider than the caller's
    /// `col_w_max`).
    pub width: f32,
}

/// Lay out `n` equal-width columns centered on the face, `col_gap` apart,
/// each capped at `col_w_max` wide. `n = 0` is the empty slice ("NO TRACKS"
/// idle plate, not a divide-by-zero). Columns are always centered on local
/// x = 0 regardless of whether the clamp leaves them narrower than the full
/// `face_w` — a lone track's column doesn't stretch to fill the wall, it
/// just sits centered at its natural width.
pub fn column_layout(n: usize, face_w: f32, col_w_max: f32, col_gap: f32) -> Vec<ColumnSlot> {
    if n == 0 {
        return Vec::new();
    }
    let n_f = n as f32;
    // Width if the columns evenly split the face (minus the gaps between
    // them); clamped so a handful of tracks doesn't turn into a handful of
    // billboards.
    let ideal = (face_w - col_gap * (n_f - 1.0)) / n_f;
    let width = ideal.min(col_w_max).max(0.0);
    let total_span = width * n_f + col_gap * (n_f - 1.0);
    let start = -total_span / 2.0 + width / 2.0;
    (0..n)
        .map(|i| ColumnSlot { x_center: start + i as f32 * (width + col_gap), width })
        .collect()
}

/// `period_us/beat` → a rounded "N BPM" readout. `0` (no clock, tempo
/// unknown) reads as `"--"` rather than a divide-by-zero or a misleading
/// `"0 BPM"`.
pub fn tempo_label(period_us: u64) -> String {
    if period_us == 0 {
        return "--".to_string();
    }
    let bpm = (60_000_000.0 / period_us as f64).round() as i64;
    format!("{bpm} BPM")
}

/// How many row entities a column needs: the smallest multiple of the
/// phrase length `beats_per_phrase` that covers at least `window_rows` plus
/// one full phrase of wrap margin (so a row that scrolls off the visible
/// window and wraps back around always re-enters exactly [`is_phrase_row`]
/// beats past where it left — no seam where phrase identity jumps).
/// `beats_per_phrase = 0` (a track with no phrase structure reported yet)
/// guards to `window_rows` itself: nothing to round to a multiple of.
pub fn row_count_for(beats_per_phrase: u64, window_rows: usize) -> usize {
    if beats_per_phrase == 0 {
        // `.max(1)`: [`row_offset`] takes this as its modulus, and a
        // zero-row column would panic there (`rem_euclid(0)`) — unreachable
        // with today's WINDOW_ROWS but cheap insurance against a retune
        // (kaibo review advisory).
        return window_rows.max(1);
    }
    let p = beats_per_phrase as usize;
    let needed = window_rows + p;
    p * needed.div_ceil(p)
}

/// Row `j`'s signed offset (in row units / beats) from the fixed playhead,
/// given the phasor's current beat position `p` — of `total` rows total,
/// `below` of which sit below the playhead (the rest above, descending
/// toward it). Wrapped into `[-below, total-below)`, so every row has
/// exactly one position on the face at all times and the set of rows tiles
/// the face without a gap or an overlap.
///
/// At `p == j` (row `j`'s own beat), the result is exactly `0.0` — row `j`
/// sits on the playhead. As `p` increases, every row's offset decreases
/// (rows drift toward and through the playhead); once a row's raw offset
/// would fall below `-below` it wraps back around to just under
/// `total - below`, re-entering at the top of the visible window.
pub fn row_offset(j: usize, p: f64, total: usize, below: f64) -> f64 {
    debug_assert!(total > 0, "row_offset: total must be positive");
    let raw = j as f64 - p + below;
    raw.rem_euclid(total as f64) - below
}

/// Whether row `j` is a phrase boundary — sound under wrapping only because
/// [`row_count_for`] guarantees `total % beats_per_phrase == 0`: a row that
/// wraps from index `total-1` back to `0` (mod `total`) lands on an index
/// that is STILL congruent to its true beat mod `beats_per_phrase`, so no
/// separate re-tagging is needed on wrap. `beats_per_phrase = 0` has no
/// phrase structure to mark.
pub fn is_phrase_row(j: usize, beats_per_phrase: u64) -> bool {
    beats_per_phrase != 0 && (j as u64).is_multiple_of(beats_per_phrase)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── tempo_label ──

    #[test]
    fn tempo_label_table() {
        assert_eq!(tempo_label(500_000), "120 BPM");
        assert_eq!(tempo_label(0), "--");
        // 60_000_000 / 1_000_000 = 60.0 exactly.
        assert_eq!(tempo_label(1_000_000), "60 BPM");
        // Rounding: 60_000_000 / 333_333 = 180.0018... -> 180.
        assert_eq!(tempo_label(333_333), "180 BPM");
        // 60_000_000 / 476_190 = 126.0001... rounds to 126.
        assert_eq!(tempo_label(476_190), "126 BPM");
    }

    // ── column_layout ──

    #[test]
    fn column_layout_empty_for_zero_tracks() {
        assert_eq!(column_layout(0, 994.0, 140.0, 12.0), Vec::new());
    }

    #[test]
    fn column_layout_single_track_centers_at_zero() {
        let cols = column_layout(1, 994.0, 140.0, 12.0);
        assert_eq!(cols.len(), 1);
        assert!((cols[0].x_center).abs() < 1e-4, "{cols:?}");
        assert!((cols[0].width - 140.0).abs() < 1e-4, "clamped to col_w_max: {cols:?}");
    }

    #[test]
    fn column_layout_eight_tracks_is_centered_and_symmetric() {
        let cols = column_layout(8, 994.0, 140.0, 12.0);
        assert_eq!(cols.len(), 8);
        // Symmetric around x = 0: slot i and its mirror (n-1-i) are negatives.
        for i in 0..4 {
            let a = cols[i].x_center;
            let b = cols[7 - i].x_center;
            assert!((a + b).abs() < 1e-3, "cols[{i}]={a} not mirrored by cols[{}]={b}", 7 - i);
        }
        // Ascending left to right.
        for w in cols.windows(2) {
            assert!(w[0].x_center < w[1].x_center);
        }
    }

    #[test]
    fn column_layout_never_exceeds_col_w_max() {
        // 2 tracks on a wide face would want ~490 each without a clamp.
        let cols = column_layout(2, 994.0, 140.0, 12.0);
        for c in &cols {
            assert!(c.width <= 140.0 + 1e-4, "{c:?}");
        }
    }

    #[test]
    fn column_layout_many_tracks_shrinks_width_to_fit() {
        // 12 tracks can't all be 140 wide on a 994-wide face; width shrinks
        // below the clamp instead of overflowing the face.
        let cols = column_layout(12, 994.0, 140.0, 12.0);
        let total_span: f32 = cols[0].width * 12.0 + 12.0 * 11.0;
        assert!(total_span <= 994.0 + 1e-2, "columns overflow the face: {total_span}");
    }

    // ── row_count_for ──

    #[test]
    fn row_count_for_is_a_multiple_of_the_phrase() {
        assert_eq!(row_count_for(4, 16) % 4, 0);
        assert_eq!(row_count_for(3, 10) % 3, 0);
        assert_eq!(row_count_for(8, 8) % 8, 0);
    }

    #[test]
    fn row_count_for_covers_the_window() {
        assert!(row_count_for(4, 16) >= 16);
        assert!(row_count_for(3, 10) >= 10);
    }

    #[test]
    fn row_count_for_adds_a_wrap_margin_beyond_the_window() {
        // A window that's already an exact multiple still gets extra rows
        // (a full phrase of margin) so wrapping has somewhere to go.
        assert!(row_count_for(4, 16) > 16, "{}", row_count_for(4, 16));
    }

    #[test]
    fn row_count_for_zero_phrase_guards_to_the_window() {
        assert_eq!(row_count_for(0, 16), 16);
    }

    #[test]
    fn row_count_for_never_returns_zero() {
        // Its result is row_offset's modulus; zero would panic there.
        assert_eq!(row_count_for(0, 0), 1);
        assert!(row_count_for(4, 0) >= 1);
    }

    // ── row_offset ──

    #[test]
    fn row_offset_is_zero_when_p_equals_j() {
        for j in 0..20usize {
            let off = row_offset(j, j as f64, 20, 12.0);
            assert!(off.abs() < 1e-9, "j={j} off={off}");
        }
    }

    #[test]
    fn row_offset_stays_within_the_wrapped_range() {
        let total = 20usize;
        let below = 12.0;
        for j in 0..total {
            for tenths in 0..300 {
                let p = tenths as f64 * 0.1;
                let off = row_offset(j, p, total, below);
                assert!(
                    off >= -below - 1e-9 && off < (total as f64 - below) + 1e-9,
                    "j={j} p={p} off={off} out of range"
                );
            }
        }
    }

    #[test]
    fn row_offset_descends_monotonically_between_wraps() {
        let total = 20usize;
        let below = 12.0;
        let j = 5usize;
        let mut prev = row_offset(j, 0.0, total, below);
        for tenths in 1..=(total as i64 * 10 - 1) {
            let p = tenths as f64 * 0.1;
            let cur = row_offset(j, p, total, below);
            // Either it kept descending, or it just wrapped (jumped back up
            // near total - below) — never anything else.
            assert!(cur < prev + 1e-6 || cur > prev + 1.0, "j={j} p={p} prev={prev} cur={cur}");
            prev = cur;
        }
    }

    #[test]
    fn row_offset_same_p_is_idempotent_the_freeze_contract() {
        // The freeze story: `animate_tracker_scroll` calls this every frame
        // with the SAME cached `p` while frozen — it must return exactly the
        // same value every time, not drift.
        let a = row_offset(7, 3.25, 20, 12.0);
        let b = row_offset(7, 3.25, 20, 12.0);
        assert_eq!(a, b);
    }

    #[test]
    fn row_offset_wrap_seam_is_continuous_across_rows() {
        // At a fixed p, walking consecutive j should walk consecutive
        // offsets (each exactly 1.0 above the previous, wrap included) —
        // the rows tile the face with no gap or overlap at the seam.
        let total = 20usize;
        let below = 12.0;
        let p = 4.6;
        let mut offs: Vec<f64> = (0..total).map(|j| row_offset(j, p, total, below)).collect();
        offs.sort_by(|a, b| a.total_cmp(b));
        for w in offs.windows(2) {
            assert!((w[1] - w[0] - 1.0).abs() < 1e-6, "{offs:?}");
        }
    }

    // ── is_phrase_row ──

    #[test]
    fn is_phrase_row_marks_every_pth_row() {
        assert!(is_phrase_row(0, 4));
        assert!(!is_phrase_row(1, 4));
        assert!(!is_phrase_row(3, 4));
        assert!(is_phrase_row(4, 4));
        assert!(is_phrase_row(8, 4));
    }

    #[test]
    fn is_phrase_row_zero_phrase_marks_nothing() {
        assert!(!is_phrase_row(0, 0));
        assert!(!is_phrase_row(4, 0));
    }

    #[test]
    fn is_phrase_row_sound_under_wrap() {
        // row_count_for guarantees total % P == 0, so a row index that wraps
        // (e.g. total + k, reduced mod total) still reads the same phrase
        // identity as k alone.
        let p = 4u64;
        let total = row_count_for(p, 16);
        assert_eq!(total % p as usize, 0);
        for k in 0..total {
            let wrapped = (total + k) % total;
            assert_eq!(is_phrase_row(wrapped, p), is_phrase_row(k, p));
        }
    }
}
