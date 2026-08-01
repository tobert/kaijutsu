//! Hunk-aware projection: fitting a diff into a budget without lying about it.
//!
//! # The contract
//!
//! 1. **Cuts land on whole-hunk boundaries.** Never mid-hunk, never mid-line.
//!    A hunk cut in half still carries a plausible `@@` header with a line
//!    count that no longer matches its body — a patch that looks real and
//!    applies wrong.
//! 2. **The result is marked incomplete.** [`crate::DiffModel::truncated`] is
//!    set, and [`crate::format`] writes the
//!    [`crate::TRUNCATION_MARKER_PREFIX`] line ahead of everything else.
//! 3. **A file whose hunks were all dropped is dropped entirely.** A bare file
//!    header with no hunks means "renamed, contents identical" in this dialect;
//!    emitting one for a file we simply ran out of room for would be a lie of a
//!    different kind.
//! 4. **Truncating an already-truncated model accumulates the counts.** The
//!    marker always reports the total distance from the complete diff.
//!
//! The budget itself is the caller's: [`crate::limits`] names the render and
//! hydration ceilings, but only the caller knows which one applies.

use crate::format::{format, format_file, format_hunk};
use crate::model::{DiffModel, FileDiff, Hunk, Truncation};

/// Bytes the truncation marker for `model` can possibly need, including its
/// newline.
///
/// Computed, not estimated. The omitted counts can never exceed what the model
/// actually holds (plus whatever a previous truncation already dropped), so
/// formatting the marker at those maxima gives a true upper bound — and a
/// bound is what the "never exceeds its budget" guarantee needs. A fixed
/// constant would have been a guess that large counts could quietly outgrow.
fn marker_reserve_bytes(model: &DiffModel) -> usize {
    let prior = model.truncated.unwrap_or_default();
    let worst = Truncation {
        omitted_files: prior.omitted_files + model.files.len(),
        omitted_hunks: prior.omitted_hunks
            + model.files.iter().map(|f| f.hunks.len()).sum::<usize>(),
        omitted_lines: prior.omitted_lines + model.line_count(),
    };
    crate::format::truncation_marker(&worst).len() + 1
}

/// Project `model` down to at most `budget` bytes of formatted output.
///
/// Returns `model` unchanged (clone) when it already fits.
///
/// # The one budget that cannot be honored
///
/// A budget smaller than the truncation marker itself has no honest answer:
/// the marker is the floor, because output that omits it would claim to be a
/// complete patch. Such a budget returns just the marker and therefore exceeds
/// the number asked for. Every budget at or above
/// [`crate::limits::MAX_HYDRATION_BYTES`] is far past that floor.
pub fn truncate_to_bytes(model: &DiffModel, budget: usize) -> DiffModel {
    // Checked against the *real* formatted size, so a model that fits exactly
    // is never truncated merely to make room for a marker it will not need.
    if format(model).len() <= budget {
        return model.clone();
    }
    project(
        model,
        budget.saturating_sub(marker_reserve_bytes(model)),
        |file| {
            let mut buf = String::new();
            let headers_only = FileDiff {
                hunks: Vec::new(),
                ..file.clone()
            };
            format_file(&mut buf, &headers_only);
            buf.len()
        },
        |hunk| {
            let mut buf = String::new();
            format_hunk(&mut buf, hunk);
            buf.len()
        },
    )
}

/// Project `model` down to at most `budget` diff body lines.
///
/// File headers are free in this accounting — the budget is about how much a
/// reader (or a viewport) has to take in, and headers are the part that makes
/// the rest legible.
pub fn truncate_to_lines(model: &DiffModel, budget: usize) -> DiffModel {
    if model.line_count() <= budget {
        return model.clone();
    }
    project(model, budget, |_| 0, |hunk| hunk.lines.len())
}

fn project(
    model: &DiffModel,
    budget: usize,
    file_cost: impl Fn(&FileDiff) -> usize,
    hunk_cost: impl Fn(&Hunk) -> usize,
) -> DiffModel {
    let mut acc = model.truncated.unwrap_or_default();
    let mut kept_files: Vec<FileDiff> = Vec::new();
    let mut used = 0usize;
    let mut stopped = false;

    for file in &model.files {
        let dropped_whole = |acc: &mut Truncation| {
            acc.omitted_files += 1;
            acc.omitted_lines += file.hunks.iter().map(|h| h.lines.len()).sum::<usize>();
        };

        let header = file_cost(file);
        if stopped || used + header > budget {
            stopped = true;
            dropped_whole(&mut acc);
            continue;
        }

        let mut kept_hunks = Vec::new();
        let mut file_used = header;
        let mut dropped_hunks = 0usize;
        let mut dropped_lines = 0usize;
        for hunk in &file.hunks {
            let cost = hunk_cost(hunk);
            if stopped || used + file_used + cost > budget {
                stopped = true;
                dropped_hunks += 1;
                dropped_lines += hunk.lines.len();
                continue;
            }
            file_used += cost;
            kept_hunks.push(hunk.clone());
        }

        if kept_hunks.is_empty() && !file.hunks.is_empty() {
            // Nothing survived; a bare header would read as a content-free
            // rename. Drop the file instead.
            dropped_whole(&mut acc);
            continue;
        }

        acc.omitted_hunks += dropped_hunks;
        acc.omitted_lines += dropped_lines;
        used += file_used;
        kept_files.push(FileDiff {
            hunks: kept_hunks,
            ..file.clone()
        });
    }

    DiffModel {
        files: kept_files,
        truncated: (!acc.is_empty()).then_some(acc),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{DiffOptions, FileSpec, diff};
    use crate::format::format;
    use crate::parse::parse;

    fn sample() -> DiffModel {
        // Two files, two well-separated hunks each.
        let before: String = (1..=60).map(|i| format!("line {i}\n")).collect();
        let a = before
            .replace("line 5\n", "FIVE\n")
            .replace("line 40\n", "FORTY\n");
        let b = before
            .replace("line 9\n", "NINE\n")
            .replace("line 50\n", "FIFTY\n");
        let specs = vec![
            FileSpec::modified("one.txt", &before, &a),
            FileSpec::modified("two.txt", &before, &b),
        ];
        diff(&specs, &DiffOptions::default()).unwrap()
    }

    #[test]
    fn the_marker_reserve_is_an_upper_bound_not_an_estimate() {
        let model = sample();
        let reserve = marker_reserve_bytes(&model);
        // Whatever budget we pick, the marker the result actually carries must
        // fit in the reserve we held back for it. A fixed constant could not
        // promise this; a bound computed from the model's own totals can.
        for budget in [0, 1, 64, 100, 200, 500, 1000, 2000] {
            let cut = truncate_to_bytes(&model, budget);
            if let Some(t) = cut.truncated {
                let marker = crate::format::truncation_marker(&t).len() + 1;
                assert!(marker <= reserve, "marker {marker} > reserve {reserve}");
            }
        }
        // And the bound is derived, not guessed: a model with counts big enough
        // to need more digits demands a bigger reserve.
        let mut huge = model.clone();
        huge.truncated = Some(Truncation {
            omitted_files: 12_345_678,
            omitted_hunks: 90_123_456,
            omitted_lines: 78_901_234,
        });
        assert!(marker_reserve_bytes(&huge) > reserve);
    }

    #[test]
    fn a_model_that_fits_comes_back_complete() {
        let model = sample();
        let out = truncate_to_bytes(&model, 1_000_000);
        assert!(out.is_complete());
        assert_eq!(out, model);
    }

    #[test]
    fn truncation_cuts_on_hunk_boundaries_and_marks_itself() {
        let model = sample();
        let full = format(&model);
        let out = truncate_to_bytes(&model, full.len() / 2);
        assert!(
            !out.is_complete(),
            "truncated model must not claim completeness"
        );
        let text = format(&out);
        assert!(text.starts_with(crate::TRUNCATION_MARKER_PREFIX));
        assert!(text.len() <= full.len() / 2);
        // Every surviving hunk is whole: re-parsing validates every `@@` count
        // against its body, so a mid-hunk cut would fail here.
        let reparsed = parse(&text).unwrap();
        assert_eq!(reparsed.truncated, out.truncated);
    }

    #[test]
    fn a_tiny_budget_yields_an_empty_but_honest_model() {
        let model = sample();
        let out = truncate_to_bytes(&model, 10);
        assert!(out.files.is_empty());
        let t = out.truncated.expect("must be marked truncated");
        assert_eq!(t.omitted_files, 2);
        assert!(t.omitted_lines > 0);
    }

    #[test]
    fn a_file_losing_every_hunk_is_dropped_whole() {
        let model = sample();
        let mut buf = String::new();
        let headers_only = FileDiff {
            hunks: Vec::new(),
            ..model.files[0].clone()
        };
        format_file(&mut buf, &headers_only);
        // Room for the first file's headers but not its first hunk.
        let out = truncate_to_bytes(&model, marker_reserve_bytes(&model) + buf.len() + 4);
        assert!(out.files.is_empty());
        assert_eq!(out.truncated.unwrap().omitted_files, 2);
    }

    #[test]
    fn line_budget_keeps_whole_hunks() {
        let model = sample();
        let out = truncate_to_lines(&model, 10);
        assert!(!out.is_complete());
        assert_eq!(
            out.line_count(),
            8,
            "one 8-line hunk fits, the next does not"
        );
    }

    #[test]
    fn truncating_twice_accumulates_the_counts() {
        let model = sample();
        let once = truncate_to_lines(&model, 24);
        let twice = truncate_to_lines(&once, 8);
        let a = once.truncated.unwrap();
        let b = twice.truncated.unwrap();
        assert!(b.omitted_lines > a.omitted_lines);
        assert_eq!(
            b.omitted_lines + twice.line_count(),
            model.line_count(),
            "the marker must report the distance from the *complete* diff"
        );
    }
}
