//! [`DiffModel`] → canonical unified-diff text.
//!
//! The canonical form is the *narrow* dialect: every file section opens with a
//! `diff --git` line, every section carries explicit `---`/`+++` headers (even
//! a rename with no content change, where git omits them), `/dev/null` marks
//! creation and deletion, counts of 1 are elided from `@@` headers exactly as
//! git elides them, and the whole thing is LF-terminated.
//!
//! Everything this module writes, [`crate::parse`] reads back into an equal
//! model. That is the property that makes the block content — plain unified
//! text on the wire — a lossless carrier for the viewer's model.

use std::fmt::Write as _;

use crate::model::{DiffModel, FileChange, FileDiff, Hunk, Truncation};
use crate::paths::quote;

/// The marker written when a model is an incomplete projection.
///
/// It leads the output rather than trailing it, so a reader — human or model —
/// knows the diff is partial *before* reading it, and so a stream cut short
/// still carries the warning. `#!` is not a valid start for any construct in
/// the dialect, which is precisely the point: truncated output must not parse
/// as, or look like, a complete patch.
pub const TRUNCATION_MARKER_PREFIX: &str = "#!kaijutsu-diff truncated: ";

/// Render the truncation marker line (without its newline).
pub fn truncation_marker(t: &Truncation) -> String {
    format!(
        "{TRUNCATION_MARKER_PREFIX}{} file(s), {} hunk(s), {} line(s) omitted",
        t.omitted_files, t.omitted_hunks, t.omitted_lines
    )
}

/// Format a whole model as canonical unified-diff text.
///
/// The result always ends with `\n` unless it is empty.
pub fn format(model: &DiffModel) -> String {
    let mut out = String::new();
    if let Some(t) = &model.truncated {
        let _ = writeln!(out, "{}", truncation_marker(t));
    }
    for file in &model.files {
        format_file(&mut out, file);
    }
    out
}

/// Format one file section into `out`.
pub fn format_file(out: &mut String, file: &FileDiff) {
    let _ = writeln!(
        out,
        "diff --git {} {}",
        quote(&format!("a/{}", file.old_path)),
        quote(&format!("b/{}", file.new_path))
    );
    if file.change == FileChange::Renamed {
        let _ = writeln!(out, "rename from {}", quote(&file.old_path));
        let _ = writeln!(out, "rename to {}", quote(&file.new_path));
    }
    match file.change {
        FileChange::Added => {
            let _ = writeln!(out, "--- /dev/null");
        }
        _ => {
            let _ = writeln!(out, "--- {}", quote(&format!("a/{}", file.old_path)));
        }
    }
    match file.change {
        FileChange::Deleted => {
            let _ = writeln!(out, "+++ /dev/null");
        }
        _ => {
            let _ = writeln!(out, "+++ {}", quote(&format!("b/{}", file.new_path)));
        }
    }
    for hunk in &file.hunks {
        format_hunk(out, hunk);
    }
}

/// Format one hunk — header and body — into `out`.
pub fn format_hunk(out: &mut String, hunk: &Hunk) {
    let _ = write!(
        out,
        "@@ -{} +{} @@",
        range(hunk.old_start, hunk.old_count()),
        range(hunk.new_start, hunk.new_count())
    );
    match &hunk.section {
        Some(section) => {
            let _ = writeln!(out, " {section}");
        }
        None => out.push('\n'),
    }
    for line in &hunk.lines {
        out.push(line.prefix());
        out.push_str(&line.text);
        out.push('\n');
        if line.no_newline {
            out.push_str("\\ No newline at end of file\n");
        }
    }
}

/// `start,count`, with `,count` elided when the count is exactly 1 — git's
/// convention, matched so external tools see nothing unusual.
fn range(start: u32, count: u32) -> String {
    if count == 1 {
        start.to_string()
    } else {
        format!("{start},{count}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{DiffOptions, FileSpec, diff_file};
    use crate::model::{DiffLine, FoldState};

    #[test]
    fn modify_renders_the_canonical_shape() {
        let file = diff_file(
            &FileSpec::modified("a.txt", "one\ntwo\n", "one\n2\n"),
            &DiffOptions::default(),
        )
        .unwrap();
        let text = format(&DiffModel::new(vec![file]));
        assert_eq!(
            text,
            "diff --git a/a.txt b/a.txt\n\
             --- a/a.txt\n\
             +++ b/a.txt\n\
             @@ -1,2 +1,2 @@\n\
             \x20one\n\
             -two\n\
             +2\n"
        );
    }

    #[test]
    fn add_and_delete_use_dev_null() {
        let added = diff_file(&FileSpec::added("new", "x\n"), &DiffOptions::default()).unwrap();
        assert!(format(&DiffModel::new(vec![added])).contains("--- /dev/null\n+++ b/new\n"));
        let deleted = diff_file(&FileSpec::deleted("old", "x\n"), &DiffOptions::default()).unwrap();
        assert!(format(&DiffModel::new(vec![deleted])).contains("--- a/old\n+++ /dev/null\n"));
    }

    #[test]
    fn rename_emits_rename_headers() {
        let file = diff_file(
            &FileSpec::renamed("old.txt", "new.txt", "same\n", "same\n"),
            &DiffOptions::default(),
        )
        .unwrap();
        let text = format(&DiffModel::new(vec![file]));
        assert_eq!(
            text,
            "diff --git a/old.txt b/new.txt\n\
             rename from old.txt\n\
             rename to new.txt\n\
             --- a/old.txt\n\
             +++ b/new.txt\n"
        );
    }

    #[test]
    fn single_line_counts_are_elided() {
        let hunk = Hunk {
            old_start: 4,
            new_start: 4,
            lines: vec![DiffLine::delete("a", 4), DiffLine::insert("b", 4)],
            section: None,
            fold: FoldState::Expanded,
        };
        let mut out = String::new();
        format_hunk(&mut out, &hunk);
        assert!(out.starts_with("@@ -4 +4 @@\n"), "{out}");
    }

    #[test]
    fn no_newline_marker_follows_its_line() {
        let file = diff_file(
            &FileSpec::modified("a", "one\n", "one"),
            &DiffOptions::default(),
        )
        .unwrap();
        let text = format(&DiffModel::new(vec![file]));
        assert!(
            text.ends_with("+one\n\\ No newline at end of file\n"),
            "{text}"
        );
    }

    #[test]
    fn truncation_marker_leads_the_output() {
        let model = DiffModel {
            files: Vec::new(),
            truncated: Some(Truncation {
                omitted_files: 2,
                omitted_hunks: 7,
                omitted_lines: 143,
            }),
        };
        assert_eq!(
            format(&model),
            "#!kaijutsu-diff truncated: 2 file(s), 7 hunk(s), 143 line(s) omitted\n"
        );
    }

    #[test]
    fn paths_with_spaces_are_quoted_everywhere_they_appear() {
        let file = diff_file(
            &FileSpec::modified("two words.txt", "a\n", "b\n"),
            &DiffOptions::default(),
        )
        .unwrap();
        let text = format(&DiffModel::new(vec![file]));
        assert!(
            text.starts_with("diff --git \"a/two words.txt\" \"b/two words.txt\"\n"),
            "{text}"
        );
        assert!(text.contains("--- \"a/two words.txt\"\n"));
    }
}
