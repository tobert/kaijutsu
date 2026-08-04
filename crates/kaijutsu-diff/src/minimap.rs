//! The minimap: a whole diff compressed into one vertical strip.
//!
//! `docs/diff.md` Decision 6 put the minimap in scope for a diff — unlike the
//! conversation, which `docs/issues.md` fences off — because a diff is a
//! **bounded** document. Bounded means the strip can show all of it at once,
//! and that is the only thing that makes a minimap honest: a scrollbar over an
//! unbounded stream is a lie about how much there is.
//!
//! Everything here is pure and knows nothing about rows, pixels, or Bevy. It
//! takes a sequence of [`MinimapClass`] — one per **drawn** row — and buckets
//! it. The viewer supplies that sequence from its visible-row projection
//! (`DiffCore::minimap`), which is what makes the strip reflect folds for
//! free: a folded hunk contributes one header row to the minimap for the same
//! reason it contributes one row to the screen.
//!
//! # Why a bucket, not a pixel
//!
//! A diff has more rows than the strip has pixels, so several rows share a
//! slot. A slot that reported only its *dominant* class would erase a single
//! deleted line inside a screenful of context — exactly the thing a reader
//! scans a minimap for. So a bucket keeps counts, and the renderer draws
//! insertions and deletions as separate bars: presence survives compression
//! even when proportion does not.
//!
//! # The mapping is invertible on purpose
//!
//! [`bucket_of`] and [`first_row_of`] are inverses (up to the rounding a
//! compression must have). Decision 6 called for `viz::scales` precisely so
//! that click-to-jump would come free from `invert`; a linear map over
//! integers is the same property with less machinery, and the inverse is here
//! and tested so wiring a click is a renderer job and not a design one.

/// What one drawn row contributes to the strip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MinimapClass {
    /// An added body line.
    Insert,
    /// A removed body line.
    Delete,
    /// An unchanged body line.
    Context,
    /// A file header, a hunk header, a truncation marker — the document's
    /// structure rather than its content. Drawn as a tick, so the reader can
    /// see where the hunks are.
    Structure,
}

/// One vertical slot of the strip: what the rows that landed in it were.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MinimapBucket {
    /// Added lines in this slot.
    pub inserts: u32,
    /// Removed lines in this slot.
    pub deletes: u32,
    /// Unchanged lines in this slot.
    pub context: u32,
    /// Header/marker rows in this slot.
    pub structure: u32,
}

impl MinimapBucket {
    /// How many rows landed here.
    pub fn rows(&self) -> u32 {
        self.inserts + self.deletes + self.context + self.structure
    }

    /// True when nothing landed here — the strip is longer than the document.
    pub fn is_empty(&self) -> bool {
        self.rows() == 0
    }

    /// How much of this slot is *change*, in `0.0..=1.0`. The bar length.
    ///
    /// Zero for an empty slot rather than a division by zero, and zero for a
    /// slot of pure context — which is the point: the eye should land on the
    /// changes.
    pub fn change_density(&self) -> f32 {
        let rows = self.rows();
        if rows == 0 {
            return 0.0;
        }
        (self.inserts + self.deletes) as f32 / rows as f32
    }
}

/// Bucket one row class per drawn row into `buckets` slots.
///
/// Rows are distributed by position, not by count: row `i` of `n` lands in
/// slot `i * buckets / n`, so the strip is a linear scale of the document and
/// [`bucket_of`] agrees with it by construction. A document shorter than the
/// strip leaves empty slots, which the renderer draws as nothing — the strip
/// showing a short diff as short is information, not a gap.
pub fn minimap_buckets<I>(classes: I, buckets: usize) -> Vec<MinimapBucket>
where
    I: IntoIterator<Item = MinimapClass>,
    I::IntoIter: ExactSizeIterator,
{
    let iter = classes.into_iter();
    let rows = iter.len();
    let mut out = vec![MinimapBucket::default(); buckets];
    if buckets == 0 || rows == 0 {
        return out;
    }
    for (i, class) in iter.enumerate() {
        let slot = bucket_of(i, rows, buckets);
        let bucket = &mut out[slot];
        match class {
            MinimapClass::Insert => bucket.inserts += 1,
            MinimapClass::Delete => bucket.deletes += 1,
            MinimapClass::Context => bucket.context += 1,
            MinimapClass::Structure => bucket.structure += 1,
        }
    }
    out
}

/// Which slot row `row` of `rows` lands in. Clamped, never out of range.
pub fn bucket_of(row: usize, rows: usize, buckets: usize) -> usize {
    if buckets == 0 {
        return 0;
    }
    if rows == 0 {
        return 0;
    }
    (row.min(rows - 1) * buckets / rows).min(buckets - 1)
}

/// The first row that lands in slot `bucket` — the inverse of [`bucket_of`],
/// and what a click on the strip should jump to.
pub fn first_row_of(bucket: usize, rows: usize, buckets: usize) -> usize {
    if buckets == 0 || rows == 0 {
        return 0;
    }
    // Ceiling division: the first row whose `bucket_of` is at least `bucket`.
    let row = bucket.min(buckets - 1) * rows;
    (row.div_ceil(buckets)).min(rows - 1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use MinimapClass::*;

    #[test]
    fn every_row_lands_in_exactly_one_bucket() {
        let classes = vec![Insert, Delete, Context, Structure, Insert, Context];
        for buckets in 1..12 {
            let out = minimap_buckets(classes.clone(), buckets);
            assert_eq!(out.len(), buckets);
            let total: u32 = out.iter().map(MinimapBucket::rows).sum();
            assert_eq!(total, classes.len() as u32, "rows lost at {buckets} buckets");
        }
    }

    /// **The property the strip exists for**: a single deleted line among a
    /// screenful of context must still be visible after compression. Counts,
    /// not a dominant class.
    #[test]
    fn a_lone_change_survives_compression() {
        let mut classes = vec![Context; 500];
        classes[123] = Delete;
        let out = minimap_buckets(classes, 40);
        let hit: Vec<&MinimapBucket> = out.iter().filter(|b| b.deletes > 0).collect();
        assert_eq!(hit.len(), 1, "the deletion vanished");
        assert!(hit[0].change_density() > 0.0);
    }

    #[test]
    fn density_is_the_changed_fraction() {
        let out = minimap_buckets(vec![Insert, Insert, Context, Context], 1);
        assert_eq!(out[0].change_density(), 0.5);
        assert_eq!(minimap_buckets(vec![Context; 4], 1)[0].change_density(), 0.0);
        assert_eq!(MinimapBucket::default().change_density(), 0.0);
    }

    /// The mapping and its inverse agree — which is what a click-to-jump
    /// would rest on.
    #[test]
    fn the_row_mapping_is_invertible() {
        for (rows, buckets) in [(1000, 40), (40, 40), (7, 40), (1, 1), (99, 7)] {
            for b in 0..buckets {
                let row = first_row_of(b, rows, buckets);
                assert!(row < rows);
                // Round-tripping never lands in a *later* bucket, and lands in
                // the same one whenever that bucket has any rows at all.
                let back = bucket_of(row, rows, buckets);
                assert!(
                    back <= b || rows < buckets,
                    "bucket {b} of {rows}/{buckets} mapped to row {row} → {back}",
                );
            }
        }
    }

    #[test]
    fn degenerate_inputs_do_not_panic() {
        assert!(minimap_buckets(Vec::<MinimapClass>::new(), 8).iter().all(MinimapBucket::is_empty));
        assert!(minimap_buckets(vec![Insert], 0).is_empty());
        assert_eq!(bucket_of(9, 0, 8), 0);
        assert_eq!(bucket_of(99, 10, 8), 7);
        assert_eq!(first_row_of(99, 10, 8), 9);
        assert_eq!(first_row_of(0, 0, 0), 0);
    }

    /// A document shorter than the strip leaves slots genuinely empty rather
    /// than smearing rows across them — a short diff should *look* short.
    #[test]
    fn a_short_document_leaves_empty_slots() {
        let out = minimap_buckets(vec![Insert, Delete, Context], 30);
        assert_eq!(out.iter().filter(|b| !b.is_empty()).count(), 3);
    }
}
