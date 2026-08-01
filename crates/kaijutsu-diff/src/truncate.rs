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

use crate::format::{format_file, format_hunk};
use crate::model::{DiffModel, FileDiff, Hunk, Truncation};

/// Bytes held back from the budget so the truncation marker itself fits.
///
/// The marker's length depends on how many digits the omitted counts take;
/// 96 bytes covers counts far past any diff a human will read.
const MARKER_RESERVE_BYTES: usize = 96;

/// Project `model` down to at most `budget` bytes of formatted output.
///
/// Returns `model` unchanged (clone) when it already fits.
pub fn truncate_to_bytes(model: &DiffModel, budget: usize) -> DiffModel {
    project(
        model,
        budget.saturating_sub(MARKER_RESERVE_BYTES),
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
        let out = truncate_to_bytes(&model, MARKER_RESERVE_BYTES + buf.len() + 4);
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
