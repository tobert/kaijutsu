//! Recursive refinement: re-diff each changed line region at word granularity.
//!
//! This is jj's `color-words` design. The line diff answers *which lines*
//! changed; a second diff over the same engine — different tokenizer — answers
//! *which words within them*. The two-pass shape matters: diffing the whole
//! file at word granularity would produce beautiful nonsense, because the line
//! structure is what makes a diff readable in the first place.
//!
//! # Regions
//!
//! A region is a run of deletions immediately followed by a run of insertions.
//! Both sides are joined with `\n`, word-diffed as a unit (so a word that moves
//! between two adjacent changed lines is still recognised), and the resulting
//! ranges are mapped back onto individual lines. A run with nothing on the
//! other side — a pure insertion or pure deletion — gets no spans, because
//! there is nothing to contrast it with.
//!
//! # Semantic cleanup
//!
//! Raw word-diff output fragments: `foo_bar` → `foo_baz` marks only `r`/`z` if
//! the tokenizer splits identifiers, and even with identifier-shaped tokens a
//! rewritten expression comes back as a shower of one-character highlights
//! around shared punctuation. Cleanup merges changed ranges separated by a
//! short or whitespace-only gap (never across a line boundary) and trims
//! whitespace off the edges of mixed ranges, so highlights stay word-shaped.
//! Ranges that are *entirely* whitespace survive untrimmed — an indentation
//! change is a real change worth seeing.

use std::ops::Range;

use imara_diff::{Algorithm, Diff, InternedInput};

use crate::limits::SEMANTIC_MERGE_GAP_BYTES;
use crate::model::{DiffLine, Hunk, LineKind, WordSpan};
use crate::tokenize::Words;

/// Recompute the word spans of every line in `hunk`.
///
/// Idempotent: existing spans are discarded first. `max_region_bytes` caps the
/// combined size of a single region (see
/// [`crate::limits::MAX_REFINE_REGION_BYTES`]); oversized regions keep line
/// granularity and empty spans.
pub fn refine_hunk(hunk: &mut Hunk, max_region_bytes: usize) {
    for line in &mut hunk.lines {
        line.words.clear();
    }

    let mut i = 0;
    while i < hunk.lines.len() {
        if hunk.lines[i].kind != LineKind::Delete {
            i += 1;
            continue;
        }
        let del_start = i;
        while i < hunk.lines.len() && hunk.lines[i].kind == LineKind::Delete {
            i += 1;
        }
        let del_len = i - del_start;
        let ins_start = i;
        while i < hunk.lines.len() && hunk.lines[i].kind == LineKind::Insert {
            i += 1;
        }
        if i > ins_start {
            refine_region(&mut hunk.lines[del_start..i], del_len, max_region_bytes);
        }
    }
}

/// Refine one delete-run/insert-run pair. `lines[..n_del]` are the deletions.
fn refine_region(lines: &mut [DiffLine], n_del: usize, max_region_bytes: usize) {
    let (dels, inss) = lines.split_at_mut(n_del);
    let before = join(dels);
    let after = join(inss);
    if before.len() + after.len() > max_region_bytes {
        return;
    }

    let (before_ranges, after_ranges) = changed_ranges(&before, &after);
    scatter(dels, &before, &before_ranges);
    scatter(inss, &after, &after_ranges);
}

fn join(lines: &[DiffLine]) -> String {
    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str(&line.text);
    }
    out
}

/// Word-diff `before` against `after`, returning cleaned-up changed byte ranges
/// on each side.
fn changed_ranges(before: &str, after: &str) -> (Vec<Range<usize>>, Vec<Range<usize>>) {
    let input = InternedInput::new(Words::new(before), Words::new(after));
    let mut diff = Diff::compute(Algorithm::Histogram, &input);
    // No indent heuristic here: it reasons about *line* indentation, which is
    // meaningless for a word stream.
    diff.postprocess_no_heuristic(&input);

    let removed = collect(before, input.before.len(), |i| diff.is_removed(i));
    let added = collect(after, input.after.len(), |i| diff.is_added(i));
    (cleanup(before, removed), cleanup(after, added))
}

/// Walk the word tokens of `text` accumulating byte offsets, keeping the ones
/// `changed` flags, and merging adjacent runs.
fn collect(text: &str, n_tokens: usize, changed: impl Fn(u32) -> bool) -> Vec<Range<usize>> {
    let mut out: Vec<Range<usize>> = Vec::new();
    let mut offset = 0usize;
    for (idx, word) in Words::new(text).enumerate() {
        debug_assert!(idx < n_tokens, "tokenizer disagreed with the interner");
        let end = offset + word.len();
        if changed(idx as u32) {
            match out.last_mut() {
                Some(last) if last.end == offset => last.end = end,
                _ => out.push(offset..end),
            }
        }
        offset = end;
    }
    out
}

/// Merge short/whitespace gaps and trim edge whitespace. See the module docs.
fn cleanup(text: &str, ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    let mut merged: Vec<Range<usize>> = Vec::with_capacity(ranges.len());
    for range in ranges {
        match merged.last_mut() {
            Some(prev) => {
                let gap = &text[prev.end..range.start];
                let mergeable = !gap.contains('\n')
                    && (gap.chars().all(char::is_whitespace)
                        || gap.len() < SEMANTIC_MERGE_GAP_BYTES);
                if mergeable {
                    prev.end = range.end;
                } else {
                    merged.push(range);
                }
            }
            None => merged.push(range),
        }
    }

    merged
        .into_iter()
        .filter_map(|range| {
            let slice = &text[range.clone()];
            if slice.chars().all(char::is_whitespace) {
                // An indentation-only change is a change; keep it whole.
                return (!slice.is_empty()).then_some(range);
            }
            let lead = slice.len() - slice.trim_start().len();
            let trail = slice.len() - slice.trim_end().len();
            let trimmed = range.start + lead..range.end - trail;
            (trimmed.start < trimmed.end).then_some(trimmed)
        })
        .collect()
}

/// Project joined-text ranges back onto the individual lines they came from.
fn scatter(lines: &mut [DiffLine], joined: &str, ranges: &[Range<usize>]) {
    let mut line_start = 0usize;
    for line in lines.iter_mut() {
        let line_end = line_start + line.text.len();
        for range in ranges {
            let start = range.start.max(line_start);
            let end = range.end.min(line_end);
            if start < end {
                line.words
                    .push(WordSpan::new(start - line_start, end - line_start));
            }
        }
        debug_assert!(joined.is_char_boundary(line_end.min(joined.len())));
        line_start = line_end + 1; // step over the joining '\n'
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::limits::MAX_REFINE_REGION_BYTES;
    use crate::model::FoldState;

    fn hunk(lines: Vec<DiffLine>) -> Hunk {
        Hunk {
            old_start: 1,
            new_start: 1,
            lines,
            section: None,
            fold: FoldState::Expanded,
        }
    }

    fn spans_of(line: &DiffLine) -> Vec<&str> {
        line.words
            .iter()
            .map(|s| &line.text[s.start..s.end])
            .collect()
    }

    #[test]
    fn one_word_changed_highlights_only_that_word() {
        let mut h = hunk(vec![
            DiffLine::delete("let alpha = 1;", 1),
            DiffLine::insert("let beta = 1;", 1),
        ]);
        refine_hunk(&mut h, MAX_REFINE_REGION_BYTES);
        assert_eq!(spans_of(&h.lines[0]), vec!["alpha"]);
        assert_eq!(spans_of(&h.lines[1]), vec!["beta"]);
    }

    #[test]
    fn cleanup_keeps_highlights_word_shaped() {
        // Without the merge step the shared `_ba` fragments this into pieces.
        let mut h = hunk(vec![
            DiffLine::delete("foo_bar_baz", 1),
            DiffLine::insert("foo_bar_qux", 1),
        ]);
        refine_hunk(&mut h, MAX_REFINE_REGION_BYTES);
        assert_eq!(spans_of(&h.lines[0]), vec!["foo_bar_baz"]);
        assert_eq!(spans_of(&h.lines[1]), vec!["foo_bar_qux"]);
    }

    #[test]
    fn indentation_only_change_survives_cleanup() {
        let mut h = hunk(vec![
            DiffLine::delete("  value", 1),
            DiffLine::insert("      value", 1),
        ]);
        refine_hunk(&mut h, MAX_REFINE_REGION_BYTES);
        assert_eq!(spans_of(&h.lines[0]).concat().trim(), "");
        assert!(
            !h.lines[1].words.is_empty(),
            "added indentation must be visible"
        );
    }

    #[test]
    fn spans_stay_inside_their_own_line_in_a_multi_line_region() {
        let mut h = hunk(vec![
            DiffLine::delete("alpha one", 1),
            DiffLine::delete("beta two", 2),
            DiffLine::insert("alpha ONE", 1),
            DiffLine::insert("beta TWO", 2),
        ]);
        refine_hunk(&mut h, MAX_REFINE_REGION_BYTES);
        for line in &h.lines {
            for span in &line.words {
                assert!(span.end <= line.text.len(), "span escaped its line");
            }
        }
        assert_eq!(spans_of(&h.lines[2]), vec!["ONE"]);
        assert_eq!(spans_of(&h.lines[3]), vec!["TWO"]);
    }

    #[test]
    fn pure_insertion_gets_no_spans() {
        let mut h = hunk(vec![
            DiffLine::context("keep", 1, 1),
            DiffLine::insert("brand new", 2),
        ]);
        refine_hunk(&mut h, MAX_REFINE_REGION_BYTES);
        assert!(h.lines.iter().all(|l| l.words.is_empty()));
    }

    #[test]
    fn oversized_region_is_left_at_line_granularity() {
        let big = "x".repeat(64);
        let mut h = hunk(vec![
            DiffLine::delete(big.clone(), 1),
            DiffLine::insert(big + "y", 1),
        ]);
        refine_hunk(&mut h, 8);
        assert!(h.lines.iter().all(|l| l.words.is_empty()));
    }

    #[test]
    fn refinement_is_idempotent() {
        let mut h = hunk(vec![
            DiffLine::delete("let alpha = 1;", 1),
            DiffLine::insert("let beta = 1;", 1),
        ]);
        refine_hunk(&mut h, MAX_REFINE_REGION_BYTES);
        let once = h.clone();
        refine_hunk(&mut h, MAX_REFINE_REGION_BYTES);
        assert_eq!(h, once);
    }
}
