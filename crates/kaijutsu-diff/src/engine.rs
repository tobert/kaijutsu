//! Generation: two versions of a file in, a [`FileDiff`] out.
//!
//! The algorithm is imara-diff's; this module is the wrapper that turns its
//! token-index hunks into unified hunks with context and line numbers, then
//! hands each hunk to [`crate::refine`] for word spans.

use imara_diff::{Algorithm, Diff, InternedInput};

use crate::error::DiffError;
use crate::limits::{DEFAULT_CONTEXT_LINES, MAX_INPUT_FILE_BYTES, MAX_REFINE_REGION_BYTES};
use crate::model::{DiffLine, DiffModel, FileChange, FileDiff, FoldState, Hunk};
use crate::profile::DiffProfile;
use crate::refine::refine_hunk;
use crate::tokenize::normalize_newlines;

/// Which edit-script algorithm to use.
///
/// `Histogram` is the default and almost always the right answer. Note that
/// **the Myers fallback is inside imara-diff**, not here: its histogram
/// implementation detects pathological inputs (tokens repeating 64+ times) and
/// drops to Myers on its own. The explicit variants exist for callers who know
/// their input shape up front — a small-alphabet token stream should ask for
/// `Myers` directly and skip the detection overhead.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DiffAlgorithm {
    /// Patience-family histogram diff. Human-readable output, fastest in
    /// practice, self-falls-back to Myers on pathological input.
    #[default]
    Histogram,
    /// Linear-space Myers with git's early-abort heuristics.
    Myers,
    /// Myers with the heuristics off: minimal edit script, pathological cost.
    MyersMinimal,
}

impl From<DiffAlgorithm> for Algorithm {
    fn from(a: DiffAlgorithm) -> Self {
        match a {
            DiffAlgorithm::Histogram => Algorithm::Histogram,
            DiffAlgorithm::Myers => Algorithm::Myers,
            DiffAlgorithm::MyersMinimal => Algorithm::MyersMinimal,
        }
    }
}

/// Knobs for [`diff`] and [`diff_file`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffOptions {
    /// Tokenizer/refinement profile. See [`DiffProfile`].
    pub profile: DiffProfile,
    /// Edit-script algorithm.
    pub algorithm: DiffAlgorithm,
    /// Unchanged lines kept on each side of a changed region.
    pub context: usize,
    /// Apply git's indent-heuristic slider correction as postprocessing.
    ///
    /// On by default: it is the difference between a diff that brackets the
    /// block you actually added and one that brackets a `}` and a blank line.
    pub indent_heuristic: bool,
    /// Per-side ceiling on input size. See [`crate::limits::MAX_INPUT_FILE_BYTES`].
    pub max_input_bytes: usize,
    /// Ceiling on a word-refined region. See [`crate::limits::MAX_REFINE_REGION_BYTES`].
    pub max_refine_region_bytes: usize,
}

impl Default for DiffOptions {
    fn default() -> Self {
        Self {
            profile: DiffProfile::default(),
            algorithm: DiffAlgorithm::default(),
            context: DEFAULT_CONTEXT_LINES,
            indent_heuristic: true,
            max_input_bytes: MAX_INPUT_FILE_BYTES,
            max_refine_region_bytes: MAX_REFINE_REGION_BYTES,
        }
    }
}

/// One file to diff: its paths, what happened to it, and both versions.
///
/// [`change`](Self::change) is an input, not a guess. The kernel knows whether
/// `apply_edit_plan` created a file; inferring "added" from an empty pre-image
/// would mislabel every edit that empties a file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSpec<'a> {
    /// Pre-image path, no `a/` prefix.
    pub old_path: &'a str,
    /// Post-image path, no `b/` prefix.
    pub new_path: &'a str,
    /// What happened to the file.
    pub change: FileChange,
    /// Pre-image content. Empty for [`FileChange::Added`].
    pub before: &'a str,
    /// Post-image content. Empty for [`FileChange::Deleted`].
    pub after: &'a str,
}

impl<'a> FileSpec<'a> {
    /// A content change at a stable path.
    pub fn modified(path: &'a str, before: &'a str, after: &'a str) -> Self {
        Self {
            old_path: path,
            new_path: path,
            change: FileChange::Modified,
            before,
            after,
        }
    }

    /// A newly created file.
    pub fn added(path: &'a str, after: &'a str) -> Self {
        Self {
            old_path: path,
            new_path: path,
            change: FileChange::Added,
            before: "",
            after,
        }
    }

    /// A removed file.
    pub fn deleted(path: &'a str, before: &'a str) -> Self {
        Self {
            old_path: path,
            new_path: path,
            change: FileChange::Deleted,
            before,
            after: "",
        }
    }

    /// A path change, with or without a content change.
    pub fn renamed(old_path: &'a str, new_path: &'a str, before: &'a str, after: &'a str) -> Self {
        Self {
            old_path,
            new_path,
            change: FileChange::Renamed,
            before,
            after,
        }
    }
}

/// Diff a whole change set.
pub fn diff(specs: &[FileSpec<'_>], options: &DiffOptions) -> Result<DiffModel, DiffError> {
    let files = specs
        .iter()
        .map(|spec| diff_file(spec, options))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(DiffModel::new(files))
}

/// Diff one file.
///
/// # Errors
///
/// - [`DiffError::TooLarge`] when either side exceeds
///   [`DiffOptions::max_input_bytes`].
/// - [`DiffError::LineEndingsOnly`] when the two sides differ *only* in line
///   terminators. CRLF is normalized on ingest, so this change would otherwise
///   produce an empty diff for a file that genuinely changed.
pub fn diff_file(spec: &FileSpec<'_>, options: &DiffOptions) -> Result<FileDiff, DiffError> {
    for (what, side) in [("pre-image", spec.before), ("post-image", spec.after)] {
        if side.len() > options.max_input_bytes {
            return Err(DiffError::TooLarge {
                what,
                limit: options.max_input_bytes,
                actual: side.len(),
            });
        }
    }

    let before = normalize_newlines(spec.before);
    let after = normalize_newlines(spec.after);
    if before == after && spec.before != spec.after {
        return Err(DiffError::LineEndingsOnly {
            path: spec.new_path.to_string(),
        });
    }

    let hunks = compute_hunks(&before, &after, options);
    Ok(FileDiff {
        old_path: spec.old_path.to_string(),
        new_path: spec.new_path.to_string(),
        change: spec.change,
        hunks,
    })
}

/// Run the line diff and assemble unified hunks with context.
fn compute_hunks(before: &str, after: &str, options: &DiffOptions) -> Vec<Hunk> {
    // `&str` tokenizes to lines *with* their trailing newline, which is how a
    // file losing its final newline registers as a change at all.
    let input = InternedInput::new(before, after);
    let mut diff = Diff::compute(options.algorithm.into(), &input);
    if options.indent_heuristic {
        diff.postprocess_lines(&input);
    } else {
        diff.postprocess_no_heuristic(&input);
    }

    let changes: Vec<imara_diff::Hunk> = diff.hunks().collect();
    if changes.is_empty() {
        return Vec::new();
    }

    let n_before = input.before.len() as u32;
    let n_after = input.after.len() as u32;
    let context = options.context as u32;
    let refine = options.profile.refines_words();

    let mut hunks = Vec::new();
    for group in group_changes(&changes, context) {
        let first = &changes[group.0];
        let last = &changes[group.1 - 1];
        let old_from = first.before.start.saturating_sub(context);
        let old_to = (last.before.end + context).min(n_before);
        let new_from = first.after.start.saturating_sub(context);
        let new_to = (last.after.end + context).min(n_after);

        let mut lines = Vec::new();
        let mut o = old_from;
        let mut n = new_from;
        for change in &changes[group.0..group.1] {
            while o < change.before.start {
                lines.push(make_line(
                    &input,
                    LineSide::Context,
                    o,
                    n,
                    n_before,
                    n_after,
                ));
                o += 1;
                n += 1;
            }
            while o < change.before.end {
                lines.push(make_line(&input, LineSide::Delete, o, n, n_before, n_after));
                o += 1;
            }
            while n < change.after.end {
                lines.push(make_line(&input, LineSide::Insert, o, n, n_before, n_after));
                n += 1;
            }
        }
        while o < old_to {
            lines.push(make_line(
                &input,
                LineSide::Context,
                o,
                n,
                n_before,
                n_after,
            ));
            o += 1;
            n += 1;
        }

        let old_count = old_to - old_from;
        let new_count = new_to - new_from;
        let mut hunk = Hunk {
            old_start: if old_count == 0 {
                old_from
            } else {
                old_from + 1
            },
            new_start: if new_count == 0 {
                new_from
            } else {
                new_from + 1
            },
            lines,
            section: None,
            fold: FoldState::Expanded,
        };
        if refine {
            refine_hunk(&mut hunk, options.max_refine_region_bytes);
        }
        hunks.push(hunk);
    }
    hunks
}

/// Group changes whose context windows would touch or overlap into single
/// hunks, returning half-open index ranges into `changes`.
fn group_changes(changes: &[imara_diff::Hunk], context: u32) -> Vec<(usize, usize)> {
    let mut groups = Vec::new();
    let mut start = 0usize;
    for i in 1..changes.len() {
        let gap = changes[i]
            .before
            .start
            .saturating_sub(changes[i - 1].before.end);
        if gap > 2 * context {
            groups.push((start, i));
            start = i;
        }
    }
    groups.push((start, changes.len()));
    groups
}

#[derive(Clone, Copy)]
enum LineSide {
    Context,
    Delete,
    Insert,
}

fn make_line(
    input: &InternedInput<&str>,
    side: LineSide,
    o: u32,
    n: u32,
    n_before: u32,
    n_after: u32,
) -> DiffLine {
    // Context lines are identical on both sides, so either token will do; take
    // the pre-image so a context line at EOF reports the pre-image's newline.
    let (token, last_of_file) = match side {
        LineSide::Context | LineSide::Delete => {
            (input.interner[input.before[o as usize]], o + 1 == n_before)
        }
        LineSide::Insert => (input.interner[input.after[n as usize]], n + 1 == n_after),
    };
    let (text, has_newline) = match token.strip_suffix('\n') {
        Some(t) => (t, true),
        None => (token, false),
    };
    let mut line = match side {
        LineSide::Context => DiffLine::context(text, o + 1, n + 1),
        LineSide::Delete => DiffLine::delete(text, o + 1),
        LineSide::Insert => DiffLine::insert(text, n + 1),
    };
    line.no_newline = last_of_file && !has_newline;
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::LineKind;

    #[test]
    fn identical_content_yields_no_hunks() {
        let spec = FileSpec::modified("a.txt", "one\ntwo\n", "one\ntwo\n");
        let file = diff_file(&spec, &DiffOptions::default()).unwrap();
        assert!(file.hunks.is_empty());
        assert_eq!(file.change, FileChange::Modified);
    }

    #[test]
    fn single_change_gets_three_lines_of_context_each_side() {
        let before = (1..=10).map(|i| format!("line {i}\n")).collect::<String>();
        let after = before.replace("line 5\n", "LINE 5\n");
        let spec = FileSpec::modified("a.txt", &before, &after);
        let file = diff_file(&spec, &DiffOptions::default()).unwrap();
        assert_eq!(file.hunks.len(), 1);
        let h = &file.hunks[0];
        assert_eq!(h.old_start, 2);
        assert_eq!(h.new_start, 2);
        assert_eq!(h.old_count(), 7);
        assert_eq!(h.new_count(), 7);
        assert_eq!(h.insertions(), 1);
        assert_eq!(h.deletions(), 1);
    }

    #[test]
    fn distant_changes_become_separate_hunks() {
        let before = (1..=40).map(|i| format!("line {i}\n")).collect::<String>();
        let after = before
            .replace("line 2\n", "two\n")
            .replace("line 30\n", "thirty\n");
        let file = diff_file(
            &FileSpec::modified("a", &before, &after),
            &DiffOptions::default(),
        )
        .unwrap();
        assert_eq!(file.hunks.len(), 2);
    }

    #[test]
    fn nearby_changes_merge_into_one_hunk() {
        let before = (1..=20).map(|i| format!("line {i}\n")).collect::<String>();
        let after = before
            .replace("line 8\n", "eight\n")
            .replace("line 12\n", "twelve\n");
        let file = diff_file(
            &FileSpec::modified("a", &before, &after),
            &DiffOptions::default(),
        )
        .unwrap();
        assert_eq!(file.hunks.len(), 1);
    }

    #[test]
    fn added_file_hunk_starts_at_zero_on_the_old_side() {
        let file = diff_file(
            &FileSpec::added("new.txt", "a\nb\n"),
            &DiffOptions::default(),
        )
        .unwrap();
        assert_eq!(file.change, FileChange::Added);
        let h = &file.hunks[0];
        assert_eq!((h.old_start, h.old_count()), (0, 0));
        assert_eq!((h.new_start, h.new_count()), (1, 2));
    }

    #[test]
    fn deleted_file_hunk_starts_at_zero_on_the_new_side() {
        let file = diff_file(
            &FileSpec::deleted("gone.txt", "a\nb\n"),
            &DiffOptions::default(),
        )
        .unwrap();
        let h = &file.hunks[0];
        assert_eq!((h.new_start, h.new_count()), (0, 0));
        assert_eq!((h.old_start, h.old_count()), (1, 2));
    }

    #[test]
    fn missing_final_newline_is_recorded_on_the_line() {
        let file = diff_file(
            &FileSpec::modified("a", "one\ntwo\n", "one\ntwo"),
            &DiffOptions::default(),
        )
        .unwrap();
        let lines = &file.hunks[0].lines;
        let inserted = lines.iter().find(|l| l.kind == LineKind::Insert).unwrap();
        assert!(inserted.no_newline);
        let deleted = lines.iter().find(|l| l.kind == LineKind::Delete).unwrap();
        assert!(!deleted.no_newline);
    }

    #[test]
    fn crlf_only_change_is_a_loud_error_not_an_empty_diff() {
        let err = diff_file(
            &FileSpec::modified("a", "x\r\ny\r\n", "x\ny\n"),
            &DiffOptions::default(),
        )
        .unwrap_err();
        assert!(matches!(err, DiffError::LineEndingsOnly { .. }));
    }

    #[test]
    fn crlf_is_normalized_out_of_the_model() {
        let file = diff_file(
            &FileSpec::modified("a", "x\r\ny\r\n", "x\r\nz\r\n"),
            &DiffOptions::default(),
        )
        .unwrap();
        assert!(file.hunks[0].lines.iter().all(|l| !l.text.contains('\r')));
    }

    #[test]
    fn oversized_input_is_rejected() {
        let big = "x".repeat(100);
        let options = DiffOptions {
            max_input_bytes: 10,
            ..Default::default()
        };
        let err = diff_file(&FileSpec::modified("a", &big, "y"), &options).unwrap_err();
        assert!(matches!(err, DiffError::TooLarge { limit: 10, .. }));
    }

    #[test]
    fn word_spans_are_computed_during_generation() {
        let file = diff_file(
            &FileSpec::modified("a", "let alpha = 1;\n", "let beta = 1;\n"),
            &DiffOptions::default(),
        )
        .unwrap();
        let inserted = file.hunks[0]
            .lines
            .iter()
            .find(|l| l.kind == LineKind::Insert)
            .unwrap();
        assert_eq!(
            inserted
                .words
                .iter()
                .map(|s| &inserted.text[s.start..s.end])
                .collect::<Vec<_>>(),
            vec!["beta"]
        );
    }
}
