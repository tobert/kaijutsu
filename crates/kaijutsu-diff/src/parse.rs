//! Unified-diff text → [`DiffModel`].
//!
//! # The accepted dialect
//!
//! Strict by construction: anything the grammar below does not name is a
//! [`DiffError`], never a skipped line. A parser that silently drops a file
//! section produces a smaller diff that still *looks* complete, which is the
//! worst possible failure for something a human reads to decide whether a
//! change is correct.
//!
//! ```text
//! diff        := truncation-marker? file*
//! file        := ("diff --git " path path extended*)? headers? hunk*
//! headers     := "--- " path "+++ " path
//! extended    := "index " … | "old mode " … | "new mode " …
//!              | "new file mode " … | "deleted file mode " …
//!              | "similarity index " … | "dissimilarity index " …
//!              | "rename from " path | "rename to " path
//! hunk        := "@@ -" range " +" range " @@" section? body
//! body        := (" " text | "-" text | "+" text | "\" marker)*
//! ```
//!
//! Accepted and *interpreted*, then dropped by canonicalization: `index`,
//! `old mode`, `new mode`, `new file mode`, `deleted file mode`,
//! `similarity index`, `dissimilarity index`. Modes and blob ids are not part
//! of this crate's model; re-formatting a parsed git diff therefore loses them.
//! This is documented lossy canonicalization, not silence — and it is exactly
//! what the "external text canonicalizes" roundtrip property asserts.
//!
//! Accepted and *preserved*: `rename from` / `rename to`, and the `@@` section
//! heading.
//!
//! **Rejected loudly**: `GIT binary patch`, `Binary files … differ`,
//! `copy from` / `copy to` (copies are not modelled), and every unrecognised
//! line.
//!
//! # Path authority
//!
//! The paths on a `diff --git` line are **advisory**. Git does not quote a path
//! for a space there, so `diff --git a/x y b/x y` cannot be split reliably.
//! Authority runs `---`/`+++` first, then `rename from`/`rename to`; a section
//! with neither is [`DiffError::MissingPaths`]. One leading `a/` or `b/` is
//! stripped when present, so a file genuinely named `a/foo` round-trips. A tab
//! and everything after it on a `---`/`+++` line is a `diff -u` timestamp and
//! is dropped.
//!
//! # Leniencies (deliberate, and short)
//!
//! - A completely empty line inside a hunk body counts as an empty context
//!   line. Mail transports and editors strip the trailing space; the hunk's
//!   declared counts make the reading unambiguous.
//! - `--- old.txt` with no `a/` prefix (plain `diff -u` output) is accepted.
//! - `@@ -1 +1 @@` and `@@ -1,1 +1,1 @@` both parse; only the former is emitted.

use crate::engine::DiffOptions;
use crate::error::DiffError;
use crate::format::TRUNCATION_MARKER_PREFIX;
use crate::limits::MAX_PARSE_BYTES;
use crate::model::{
    DiffLine, DiffModel, FileChange, FileDiff, FoldState, Hunk, LineKind, Truncation,
};
use crate::paths::unquote;
use crate::refine::refine_hunk;
use crate::tokenize::normalize_newlines;

/// The stable head of the no-newline marker. Producers vary after this point
/// ("at end of file" vs svn's "at end of property"), so only this is required.
const NO_NEWLINE_MARKER_HEAD: &str = "\\ No newline";

/// Parse unified-diff text with default options.
pub fn parse(text: &str) -> Result<DiffModel, DiffError> {
    parse_with(text, &DiffOptions::default())
}

/// Parse unified-diff text, recomputing word spans under `options`.
///
/// Word spans are **not** carried by the text; they are derived here with the
/// same refinement the generator used, which is what makes format→parse an
/// identity on models.
pub fn parse_with(text: &str, options: &DiffOptions) -> Result<DiffModel, DiffError> {
    if text.len() > MAX_PARSE_BYTES {
        return Err(DiffError::TooLarge {
            what: "diff text",
            limit: MAX_PARSE_BYTES,
            actual: text.len(),
        });
    }
    let normalized = normalize_newlines(text);
    if normalized.is_empty() {
        return Ok(DiffModel::default());
    }
    let body = normalized.strip_suffix('\n').unwrap_or(&normalized);
    let lines: Vec<&str> = body.split('\n').collect();

    let mut p = Parser {
        lines: &lines,
        at: 0,
        options,
    };
    p.parse_model()
}

struct Parser<'a> {
    lines: &'a [&'a str],
    at: usize,
    options: &'a DiffOptions,
}

/// A `---`/`+++` header path, after prefix stripping.
enum HeaderPath {
    DevNull,
    Path(String),
}

impl<'a> Parser<'a> {
    fn peek(&self) -> Option<&'a str> {
        self.lines.get(self.at).copied()
    }

    /// 1-based line number of the current position, for error messages.
    fn lineno(&self) -> usize {
        self.at + 1
    }

    fn parse_model(&mut self) -> Result<DiffModel, DiffError> {
        let truncated = self.parse_truncation_marker()?;
        let mut files = Vec::new();
        while let Some(line) = self.peek() {
            if line.starts_with("diff --git ") || line.starts_with("--- ") {
                files.push(self.parse_file()?);
            } else if is_binary_marker(line) {
                return Err(DiffError::BinaryPatch {
                    line: self.lineno(),
                    found: line.to_string(),
                });
            } else {
                return Err(DiffError::ExpectedFileHeader {
                    line: self.lineno(),
                    found: line.to_string(),
                });
            }
        }
        Ok(DiffModel { files, truncated })
    }

    fn parse_truncation_marker(&mut self) -> Result<Option<Truncation>, DiffError> {
        let Some(line) = self.peek() else {
            return Ok(None);
        };
        let Some(rest) = line.strip_prefix(TRUNCATION_MARKER_PREFIX) else {
            return Ok(None);
        };
        let lineno = self.lineno();
        let malformed = move || DiffError::MalformedTruncationMarker {
            line: lineno,
            found: line.to_string(),
        };
        let mut counts = rest.split(", ").map(|part| {
            part.split_whitespace()
                .next()
                .and_then(|n| n.parse::<usize>().ok())
                .ok_or_else(malformed)
        });
        let omitted_files = counts.next().ok_or_else(malformed)??;
        let omitted_hunks = counts.next().ok_or_else(malformed)??;
        let omitted_lines = counts.next().ok_or_else(malformed)??;
        if counts.next().is_some() {
            return Err(malformed());
        }
        self.at += 1;
        Ok(Some(Truncation {
            omitted_files,
            omitted_hunks,
            omitted_lines,
        }))
    }

    fn parse_file(&mut self) -> Result<FileDiff, DiffError> {
        let section_line = self.lineno();
        let mut rename: Option<(String, String)> = None;
        let mut declared: Option<FileChange> = None;

        if self.peek().is_some_and(|l| l.starts_with("diff --git ")) {
            self.at += 1;
            self.parse_extended_headers(&mut rename, &mut declared)?;
        }

        // ── authoritative paths ─────────────────────────────────────────────
        let mut old_path;
        let mut new_path;
        let mut change;
        if self.peek().is_some_and(|l| l.starts_with("--- ")) {
            let line = self.lineno();
            let pre = self.parse_header_path("--- ", "a/")?;
            let Some(next) = self.peek() else {
                return Err(DiffError::MissingPostImageHeader {
                    line: self.lineno(),
                    found: String::new(),
                });
            };
            if !next.starts_with("+++ ") {
                return Err(DiffError::MissingPostImageHeader {
                    line: self.lineno(),
                    found: next.to_string(),
                });
            }
            let post = self.parse_header_path("+++ ", "b/")?;
            match (pre, post) {
                (HeaderPath::DevNull, HeaderPath::DevNull) => {
                    return Err(DiffError::ConflictingFileHeaders {
                        line,
                        detail: "both sides are /dev/null".to_string(),
                    });
                }
                (HeaderPath::DevNull, HeaderPath::Path(p)) => {
                    old_path = p.clone();
                    new_path = p;
                    change = FileChange::Added;
                }
                (HeaderPath::Path(p), HeaderPath::DevNull) => {
                    old_path = p.clone();
                    new_path = p;
                    change = FileChange::Deleted;
                }
                (HeaderPath::Path(a), HeaderPath::Path(b)) => {
                    change = if a == b {
                        FileChange::Modified
                    } else {
                        FileChange::Renamed
                    };
                    old_path = a;
                    new_path = b;
                }
            }
        } else if let Some((from, to)) = rename.clone() {
            old_path = from;
            new_path = to;
            change = FileChange::Renamed;
        } else {
            return Err(DiffError::MissingPaths { line: section_line });
        }

        if let Some((from, to)) = rename {
            if change != FileChange::Added && change != FileChange::Deleted {
                if old_path != from || new_path != to {
                    return Err(DiffError::ConflictingFileHeaders {
                        line: section_line,
                        detail: format!(
                            "rename headers say {from:?} → {to:?} but the file headers say \
                             {old_path:?} → {new_path:?}"
                        ),
                    });
                }
                change = FileChange::Renamed;
            } else {
                return Err(DiffError::ConflictingFileHeaders {
                    line: section_line,
                    detail: "rename headers beside a /dev/null side".to_string(),
                });
            }
        }
        if let Some(declared) = declared
            && declared != change
        {
            return Err(DiffError::ConflictingFileHeaders {
                line: section_line,
                detail: format!("mode header says {declared:?}, path headers say {change:?}"),
            });
        }

        // ── hunks ───────────────────────────────────────────────────────────
        let mut hunks = Vec::new();
        while self.peek().is_some_and(|l| l.starts_with("@@")) {
            hunks.push(self.parse_hunk()?);
        }
        // A rename with no content change legitimately has zero hunks; so does
        // a mode-only change in an external diff. Both are kept as-is.
        old_path.shrink_to_fit();
        new_path.shrink_to_fit();
        Ok(FileDiff {
            old_path,
            new_path,
            change,
            hunks,
        })
    }

    fn parse_extended_headers(
        &mut self,
        rename: &mut Option<(String, String)>,
        declared: &mut Option<FileChange>,
    ) -> Result<(), DiffError> {
        let mut from: Option<String> = None;
        let mut to: Option<String> = None;
        while let Some(line) = self.peek() {
            if line.starts_with("--- ") || line.starts_with("@@") || line.starts_with("diff --git ")
            {
                break;
            }
            let lineno = self.lineno();
            if is_binary_marker(line) {
                return Err(DiffError::BinaryPatch {
                    line: lineno,
                    found: line.to_string(),
                });
            }
            if let Some(rest) = line.strip_prefix("rename from ") {
                from = Some(unquote(rest, lineno)?);
            } else if let Some(rest) = line.strip_prefix("rename to ") {
                to = Some(unquote(rest, lineno)?);
            } else if line.starts_with("new file mode ") {
                *declared = Some(FileChange::Added);
            } else if line.starts_with("deleted file mode ") {
                *declared = Some(FileChange::Deleted);
            } else if [
                "index ",
                "old mode ",
                "new mode ",
                "similarity index ",
                "dissimilarity index ",
            ]
            .iter()
            .any(|p| line.starts_with(p))
            {
                // Recognised, interpreted as "nothing this crate models",
                // dropped by canonicalization. See the module docs.
            } else {
                return Err(DiffError::UnsupportedExtension {
                    line: lineno,
                    found: line.to_string(),
                });
            }
            self.at += 1;
        }
        match (from, to) {
            (Some(f), Some(t)) => *rename = Some((f, t)),
            (None, None) => {}
            _ => {
                return Err(DiffError::ConflictingFileHeaders {
                    line: self.lineno(),
                    detail: "only one of `rename from` / `rename to` present".to_string(),
                });
            }
        }
        Ok(())
    }

    fn parse_header_path(&mut self, prefix: &str, strip: &str) -> Result<HeaderPath, DiffError> {
        let lineno = self.lineno();
        let line = self.peek().expect("caller checked");
        let rest = &line[prefix.len()..];
        // A quoted path holds any tab itself; an unquoted one is followed by a
        // `diff -u` timestamp after a tab.
        let token = if rest.starts_with('"') {
            rest
        } else {
            rest.split('\t').next().unwrap_or(rest)
        };
        let path = unquote(token, lineno)?;
        self.at += 1;
        if path == "/dev/null" {
            return Ok(HeaderPath::DevNull);
        }
        Ok(HeaderPath::Path(
            path.strip_prefix(strip).unwrap_or(&path).to_string(),
        ))
    }

    /// Count body lines from the current position that a satisfied hunk did not
    /// account for, as `(old, new)`.
    ///
    /// Scanning stops at a line that starts a hunk or a file section, or at
    /// anything that is not a body line. A line beginning `--- ` is treated as
    /// a section start, not a deletion: the declared counts are what
    /// disambiguate the two, and by the time this runs they say the hunk is
    /// over. Both sides are counted so the error can name both — a context line
    /// overruns *both*, and reporting only one of them tells half a truth.
    fn scan_body_overrun(&self) -> (u32, u32) {
        let mut old_extra = 0u32;
        let mut new_extra = 0u32;
        for line in &self.lines[self.at..] {
            if line.starts_with("@@") || line.starts_with("diff --git ") || line.starts_with("--- ")
            {
                break;
            }
            match line.chars().next() {
                None | Some(' ') => {
                    old_extra += 1;
                    new_extra += 1;
                }
                Some('-') => old_extra += 1,
                Some('+') => new_extra += 1,
                Some('\\') => {}
                Some(_) => break,
            }
        }
        (old_extra, new_extra)
    }

    /// Consume a `\ No newline …` marker and attach it to the last body line.
    ///
    /// The content is checked, not just the `\` prefix: `\` means exactly one
    /// thing in this dialect, and accepting `\ anything` would let a malformed
    /// line silently flip `no_newline` — a one-character difference in the
    /// emitted patch that nothing downstream could see. The trailing wording
    /// varies between producers (git says "at end of file", svn says "at end of
    /// property"), so only the stable head is required.
    fn take_no_newline_marker(&mut self, lines: &mut [DiffLine]) -> Result<(), DiffError> {
        let line = self.lineno();
        let raw = self.peek().expect("caller checked");
        if !raw.starts_with(NO_NEWLINE_MARKER_HEAD) {
            return Err(DiffError::UnexpectedHunkLine {
                line,
                found: raw.to_string(),
            });
        }
        let last = lines.last_mut().ok_or(DiffError::StrayNoNewline { line })?;
        last.no_newline = true;
        self.at += 1;
        Ok(())
    }

    fn parse_hunk(&mut self) -> Result<Hunk, DiffError> {
        let header_line = self.lineno();
        let raw = self.peek().expect("caller checked");
        let malformed = || DiffError::MalformedHunkHeader {
            line: header_line,
            found: raw.to_string(),
        };

        let rest = raw.strip_prefix("@@ ").ok_or_else(malformed)?;
        let (ranges, tail) = rest.split_once(" @@").ok_or_else(malformed)?;
        let section = match tail.strip_prefix(' ') {
            Some(s) if !s.is_empty() => Some(s.to_string()),
            Some(_) => None,
            None if tail.is_empty() => None,
            None => return Err(malformed()),
        };
        let (old_spec, new_spec) = ranges.split_once(' ').ok_or_else(malformed)?;
        let (old_start, old_count) = parse_range(old_spec, '-').ok_or_else(malformed)?;
        let (new_start, new_count) = parse_range(new_spec, '+').ok_or_else(malformed)?;
        self.at += 1;

        let mut lines: Vec<DiffLine> = Vec::new();
        let mut old_left = old_count;
        let mut new_left = new_count;
        let mut old_no = old_start.max(1);
        let mut new_no = new_start.max(1);
        while old_left > 0 || new_left > 0 {
            let lineno = self.lineno();
            let Some(raw_line) = self.peek() else {
                // At EOF there is nothing left to scan, so what we consumed is
                // exactly what the body held.
                return Err(DiffError::HunkCountMismatch {
                    line: header_line,
                    declared_old: old_count,
                    actual_old: old_count - old_left,
                    declared_new: new_count,
                    actual_new: new_count - new_left,
                });
            };
            let (kind, text) = match raw_line.chars().next() {
                None => (LineKind::Context, ""),
                Some(' ') => (LineKind::Context, &raw_line[1..]),
                Some('-') => (LineKind::Delete, &raw_line[1..]),
                Some('+') => (LineKind::Insert, &raw_line[1..]),
                Some('\\') => {
                    self.take_no_newline_marker(&mut lines)?;
                    continue;
                }
                Some(_) => {
                    return Err(DiffError::UnexpectedHunkLine {
                        line: lineno,
                        found: raw_line.to_string(),
                    });
                }
            };
            let consumes_old = matches!(kind, LineKind::Context | LineKind::Delete);
            let consumes_new = matches!(kind, LineKind::Context | LineKind::Insert);
            if (consumes_old && old_left == 0) || (consumes_new && new_left == 0) {
                // One side is already full while the other is not. Scan the
                // rest of the body so the error reports what the hunk really
                // held on *both* sides rather than where we happened to stop.
                let (extra_old, extra_new) = self.scan_body_overrun();
                return Err(DiffError::HunkCountMismatch {
                    line: header_line,
                    declared_old: old_count,
                    actual_old: old_count - old_left + extra_old,
                    declared_new: new_count,
                    actual_new: new_count - new_left + extra_new,
                });
            }
            let mut line = match kind {
                LineKind::Context => DiffLine::context(text, old_no, new_no),
                LineKind::Delete => DiffLine::delete(text, old_no),
                LineKind::Insert => DiffLine::insert(text, new_no),
            };
            line.words.clear();
            if consumes_old {
                old_left -= 1;
                old_no += 1;
            }
            if consumes_new {
                new_left -= 1;
                new_no += 1;
            }
            lines.push(line);
            self.at += 1;
        }
        // A trailing `\ No newline` belongs to the hunk's last line.
        if self.peek().is_some_and(|l| l.starts_with('\\')) {
            self.take_no_newline_marker(&mut lines)?;
        }
        // Body lines beyond the declared counts would otherwise be misread as
        // the next file section. Catch the over-count here, where we can name
        // it — and count how far the overrun actually goes on each side, so the
        // error says something true rather than "at least one more".
        let (extra_old, extra_new) = self.scan_body_overrun();
        if extra_old > 0 || extra_new > 0 {
            return Err(DiffError::HunkCountMismatch {
                line: header_line,
                declared_old: old_count,
                actual_old: old_count + extra_old,
                declared_new: new_count,
                actual_new: new_count + extra_new,
            });
        }

        let mut hunk = Hunk {
            old_start,
            new_start,
            lines,
            section,
            fold: FoldState::Expanded,
        };
        if self.options.profile.refines_words() {
            refine_hunk(&mut hunk, self.options.max_refine_region_bytes);
        }
        Ok(hunk)
    }
}

/// `-12,3` / `+12` → `(12, 3)` / `(12, 1)`.
fn parse_range(spec: &str, sign: char) -> Option<(u32, u32)> {
    let spec = spec.strip_prefix(sign)?;
    match spec.split_once(',') {
        Some((start, count)) => Some((start.parse().ok()?, count.parse().ok()?)),
        None => Some((spec.parse().ok()?, 1)),
    }
}

fn is_binary_marker(line: &str) -> bool {
    line == "GIT binary patch"
        || (line.starts_with("Binary files ") && line.ends_with(" differ"))
        || line.starts_with("Files ") && line.ends_with(" differ")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::format;

    fn one(text: &str) -> FileDiff {
        let model = parse(text).unwrap();
        assert_eq!(model.files.len(), 1);
        model.files.into_iter().next().unwrap()
    }

    #[test]
    fn empty_input_is_an_empty_model_not_an_error() {
        assert_eq!(parse("").unwrap(), DiffModel::default());
    }

    #[test]
    fn plain_diff_u_output_without_git_headers_parses() {
        let file = one("--- old.txt\t2026-08-01 10:00:00.000000000 +0000\n\
             +++ new.txt\t2026-08-01 10:00:01.000000000 +0000\n\
             @@ -1 +1 @@\n\
             -a\n\
             +b\n");
        assert_eq!(file.old_path, "old.txt");
        assert_eq!(file.new_path, "new.txt");
        assert_eq!(file.change, FileChange::Renamed);
    }

    #[test]
    fn dev_null_marks_add_and_delete() {
        let added = one("--- /dev/null\n+++ b/new\n@@ -0,0 +1 @@\n+x\n");
        assert_eq!(added.change, FileChange::Added);
        assert_eq!(added.old_path, "new");
        let deleted = one("--- a/old\n+++ /dev/null\n@@ -1 +0,0 @@\n-x\n");
        assert_eq!(deleted.change, FileChange::Deleted);
    }

    #[test]
    fn a_slash_prefix_is_stripped_exactly_once() {
        let file = one("--- a/a/foo\n+++ b/a/foo\n@@ -1 +1 @@\n-x\n+y\n");
        assert_eq!(file.old_path, "a/foo");
    }

    #[test]
    fn rename_headers_are_preserved() {
        let file = one("diff --git a/old b/new\n\
             similarity index 100%\n\
             rename from old\n\
             rename to new\n\
             --- a/old\n+++ b/new\n");
        assert_eq!(file.change, FileChange::Renamed);
        assert_eq!(
            (file.old_path.as_str(), file.new_path.as_str()),
            ("old", "new")
        );
        assert!(file.hunks.is_empty());
    }

    #[test]
    fn pure_rename_without_file_headers_parses() {
        let file =
            one("diff --git a/old b/new\nsimilarity index 100%\nrename from old\nrename to new\n");
        assert_eq!(file.change, FileChange::Renamed);
        assert_eq!(file.new_path, "new");
    }

    #[test]
    fn index_and_mode_headers_are_accepted_then_canonicalized_away() {
        let text = "diff --git a/x b/x\nindex 0123456..789abcd 100644\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n";
        let model = parse(text).unwrap();
        assert!(!format(&model).contains("index "));
    }

    #[test]
    fn empty_line_in_a_body_is_an_empty_context_line() {
        let file = one("--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n\n-c\n+C\n");
        assert_eq!(file.hunks[0].lines[1].kind, LineKind::Context);
        assert_eq!(file.hunks[0].lines[1].text, "");
    }

    #[test]
    fn no_newline_marker_attaches_to_the_preceding_line() {
        let file = one("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n\\ No newline at end of file\n+b\n");
        assert!(file.hunks[0].lines[0].no_newline);
        assert!(!file.hunks[0].lines[1].no_newline);
    }

    #[test]
    fn trailing_no_newline_marker_attaches_to_the_last_line() {
        let file = one("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n\\ No newline at end of file\n");
        assert!(file.hunks[0].lines[1].no_newline);
    }

    #[test]
    fn word_spans_are_recomputed_on_parse() {
        let file = one("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-let alpha = 1;\n+let beta = 1;\n");
        let ins = &file.hunks[0].lines[1];
        assert_eq!(
            ins.words
                .iter()
                .map(|s| &ins.text[s.start..s.end])
                .collect::<Vec<_>>(),
            vec!["beta"]
        );
    }

    #[test]
    fn quoted_paths_are_decoded() {
        let file = one("--- \"a/two words.txt\"\n+++ \"b/two words.txt\"\n@@ -1 +1 @@\n-a\n+b\n");
        assert_eq!(file.old_path, "two words.txt");
    }

    #[test]
    fn section_heading_survives() {
        let file = one("--- a/x\n+++ b/x\n@@ -1 +1 @@ fn main()\n-a\n+b\n");
        assert_eq!(file.hunks[0].section.as_deref(), Some("fn main()"));
    }

    #[test]
    fn multi_file_sections_split_on_the_next_header() {
        let model = parse(
            "--- a/one\n+++ b/one\n@@ -1 +1 @@\n-a\n+b\n\
             --- a/two\n+++ b/two\n@@ -1 +1 @@\n-c\n+d\n",
        )
        .unwrap();
        assert_eq!(model.files.len(), 2);
    }

    // ── loud rejections ─────────────────────────────────────────────────────

    #[test]
    fn git_binary_patch_is_rejected() {
        let err =
            parse("diff --git a/x.png b/x.png\nindex a..b 100644\nGIT binary patch\nliteral 12\n")
                .unwrap_err();
        assert!(matches!(err, DiffError::BinaryPatch { .. }), "{err:?}");
    }

    #[test]
    fn binary_files_differ_is_rejected() {
        let err = parse("Binary files a/x.png and b/x.png differ\n").unwrap_err();
        assert!(matches!(err, DiffError::BinaryPatch { .. }), "{err:?}");
    }

    #[test]
    fn copy_headers_are_rejected_because_copies_are_not_modelled() {
        let err =
            parse("diff --git a/x b/y\ncopy from x\ncopy to y\n--- a/x\n+++ b/y\n").unwrap_err();
        assert!(
            matches!(err, DiffError::UnsupportedExtension { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn garbage_preamble_is_rejected() {
        let err = parse("From 0123 Mon Sep 17 00:00:00 2001\n--- a/x\n+++ b/x\n").unwrap_err();
        assert!(
            matches!(err, DiffError::ExpectedFileHeader { line: 1, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn malformed_hunk_header_is_rejected() {
        let err = parse("--- a/x\n+++ b/x\n@@ nonsense @@\n-a\n+b\n").unwrap_err();
        assert!(
            matches!(err, DiffError::MalformedHunkHeader { line: 3, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn short_hunk_body_is_rejected_and_reports_both_sides() {
        let err = parse("--- a/x\n+++ b/x\n@@ -1,3 +1,3 @@\n a\n-b\n+B\n").unwrap_err();
        assert_eq!(
            err,
            DiffError::HunkCountMismatch {
                line: 3,
                declared_old: 3,
                actual_old: 2,
                declared_new: 3,
                actual_new: 2,
            }
        );
    }

    #[test]
    fn long_hunk_body_is_rejected_and_reports_both_sides() {
        let err = parse("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n+c\n+d\n").unwrap_err();
        assert_eq!(
            err,
            DiffError::HunkCountMismatch {
                line: 3,
                declared_old: 1,
                actual_old: 1,
                declared_new: 1,
                actual_new: 3,
            }
        );
    }

    #[test]
    fn a_context_line_overrun_is_reported_on_both_sides() {
        // A context line belongs to both sides; the old reporting named only
        // one of them and left the reader guessing about the other.
        let err = parse("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n c\n d\n").unwrap_err();
        assert_eq!(
            err,
            DiffError::HunkCountMismatch {
                line: 3,
                declared_old: 1,
                actual_old: 3,
                declared_new: 1,
                actual_new: 3,
            }
        );
    }

    #[test]
    fn missing_post_image_header_is_rejected() {
        let err = parse("--- a/x\n@@ -1 +1 @@\n-a\n+b\n").unwrap_err();
        assert!(
            matches!(err, DiffError::MissingPostImageHeader { .. }),
            "{err:?}"
        );
    }

    #[test]
    fn file_section_without_any_paths_is_rejected() {
        let err = parse("diff --git a/x b/x\nindex a..b 100644\n").unwrap_err();
        assert!(
            matches!(err, DiffError::MissingPaths { line: 1 }),
            "{err:?}"
        );
    }

    #[test]
    fn a_backslash_line_that_is_not_the_marker_is_rejected() {
        // `\` is only ever the no-newline marker in this dialect. Accepting
        // any `\`-prefixed line would let `\ garbage` silently set no_newline
        // — a one-character difference in the emitted patch, invisible.
        let err = parse("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n\\ garbage\n+b\n").unwrap_err();
        assert!(
            matches!(err, DiffError::UnexpectedHunkLine { line: 5, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn a_trailing_backslash_line_that_is_not_the_marker_is_rejected() {
        let err = parse("--- a/x\n+++ b/x\n@@ -1 +1 @@\n-a\n+b\n\\ nonsense\n").unwrap_err();
        assert!(
            matches!(err, DiffError::UnexpectedHunkLine { line: 6, .. }),
            "{err:?}"
        );
    }

    #[test]
    fn stray_no_newline_marker_is_rejected() {
        let err = parse("--- a/x\n+++ b/x\n@@ -1 +1 @@\n\\ No newline at end of file\n-a\n+b\n")
            .unwrap_err();
        assert!(
            matches!(err, DiffError::StrayNoNewline { line: 4 }),
            "{err:?}"
        );
    }

    #[test]
    fn oversized_input_is_rejected() {
        let text = "x".repeat(MAX_PARSE_BYTES + 1);
        assert!(matches!(parse(&text), Err(DiffError::TooLarge { .. })));
    }
}
