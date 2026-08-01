//! The diff model: files → hunks → lines, with intra-line word spans.
//!
//! One model, many projections. Unified rendering ships first and side-by-side
//! comes later, but both are *views* of the same [`Hunk::lines`] sequence: a
//! unified view walks it in order, a side-by-side view walks it twice filtering
//! on [`LineKind`]. Nothing here is unified-specific except the line numbers,
//! which both projections need anyway.

use crate::stat::DiffStat;

/// What happened to a file between the two sides of the diff.
///
/// This is **supplied by the caller**, never inferred from empty content — an
/// empty file that stays empty is not a deletion, and guessing would be exactly
/// the kind of silent fallback that corrupts a patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileChange {
    /// Content changed; the path did not.
    #[default]
    Modified,
    /// The file did not exist before. Formats as `--- /dev/null`.
    Added,
    /// The file does not exist after. Formats as `+++ /dev/null`.
    Deleted,
    /// The path changed. Content may also have changed.
    ///
    /// Renames are **represented, not detected**: the parser reads git's
    /// `rename from` / `rename to` headers and the formatter writes them back,
    /// but nothing in this crate performs similarity detection. That arrives
    /// with the git-backed sources (`gix-diff`'s rename tracking).
    Renamed,
}

/// One line of one hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffLine {
    /// Context, insertion, or deletion.
    pub kind: LineKind,
    /// Line content **without** the `+`/`-`/space prefix and **without** its
    /// trailing newline. Never contains `\r` — CRLF is normalized on ingest.
    pub text: String,
    /// True when this line is the last of its file and that file has no final
    /// newline. Formats as a following `\ No newline at end of file`.
    pub no_newline: bool,
    /// Byte ranges within [`text`](Self::text) that differ from the paired
    /// line on the other side — the word-level refinement.
    ///
    /// Empty on context lines, on unrefined regions (see
    /// [`crate::limits::MAX_REFINE_REGION_BYTES`]), and on pure
    /// insertions/deletions that have no counterpart to compare against.
    /// Spans are **derived**, not serialized: they are recomputed by
    /// [`crate::parse`], which is what makes the format/parse roundtrip an
    /// identity on models.
    pub words: Vec<WordSpan>,
    /// 1-based line number on the pre-image side. `None` on insertions.
    pub old_lineno: Option<u32>,
    /// 1-based line number on the post-image side. `None` on deletions.
    pub new_lineno: Option<u32>,
}

impl DiffLine {
    /// A context line at the given pre/post line numbers.
    pub fn context(text: impl Into<String>, old_lineno: u32, new_lineno: u32) -> Self {
        Self {
            kind: LineKind::Context,
            text: text.into(),
            no_newline: false,
            words: Vec::new(),
            old_lineno: Some(old_lineno),
            new_lineno: Some(new_lineno),
        }
    }

    /// A deleted line at the given pre-image line number.
    pub fn delete(text: impl Into<String>, old_lineno: u32) -> Self {
        Self {
            kind: LineKind::Delete,
            text: text.into(),
            no_newline: false,
            words: Vec::new(),
            old_lineno: Some(old_lineno),
            new_lineno: None,
        }
    }

    /// An inserted line at the given post-image line number.
    pub fn insert(text: impl Into<String>, new_lineno: u32) -> Self {
        Self {
            kind: LineKind::Insert,
            text: text.into(),
            no_newline: false,
            words: Vec::new(),
            old_lineno: None,
            new_lineno: Some(new_lineno),
        }
    }

    /// The single-character unified-diff prefix for this line.
    pub fn prefix(&self) -> char {
        match self.kind {
            LineKind::Context => ' ',
            LineKind::Delete => '-',
            LineKind::Insert => '+',
        }
    }
}

/// Whether a line is unchanged, added, or removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// Present on both sides.
    Context,
    /// Present only on the post-image side.
    Insert,
    /// Present only on the pre-image side.
    Delete,
}

/// A byte range within a [`DiffLine::text`] that the word-level refinement
/// marked as changed.
///
/// Ranges are half-open, non-overlapping, and sorted ascending. They index
/// **bytes**, not chars — `text[span.start..span.end]` is always a valid UTF-8
/// slice because the word tokenizer only ever splits on char boundaries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct WordSpan {
    /// Inclusive start byte offset.
    pub start: usize,
    /// Exclusive end byte offset.
    pub end: usize,
}

impl WordSpan {
    /// Construct a span. Debug-asserts that it is non-empty and ordered.
    pub fn new(start: usize, end: usize) -> Self {
        debug_assert!(start < end, "word spans must be non-empty");
        Self { start, end }
    }

    /// Length of the span in bytes.
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    /// True when the span covers no bytes. Never true for spans this crate
    /// produces; present because clippy asks for it beside `len`.
    pub fn is_empty(&self) -> bool {
        self.start >= self.end
    }
}

/// Whether a hunk is drawn expanded or collapsed.
///
/// **View state, not content.** [`crate::format`] ignores it and
/// [`crate::parse`] always produces [`FoldState::Expanded`], so a model that
/// has been folded will not compare equal to its own format/parse roundtrip.
/// It lives on the model rather than in the viewer because folds are a
/// property of the hunk (a fold survives a scroll, a resize, and a projection
/// switch between unified and side-by-side).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FoldState {
    /// All lines drawn.
    #[default]
    Expanded,
    /// Only the hunk header drawn.
    Folded,
}

/// A contiguous changed region of one file, with its surrounding context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hunk {
    /// 1-based pre-image start line, or `0` when [`old_count`](Self::old_count)
    /// is zero (git's convention for "inserted before line 1").
    pub old_start: u32,
    /// 1-based post-image start line, or `0` when the post-image count is zero.
    pub new_start: u32,
    /// The lines of the hunk, in unified order.
    pub lines: Vec<DiffLine>,
    /// The optional section heading git writes after the closing `@@`
    /// (typically the enclosing function). Preserved on parse, never generated.
    pub section: Option<String>,
    /// Fold state. See [`FoldState`] — view state, not content.
    pub fold: FoldState,
}

impl Hunk {
    /// Number of pre-image lines: context + deletions.
    pub fn old_count(&self) -> u32 {
        self.lines
            .iter()
            .filter(|l| matches!(l.kind, LineKind::Context | LineKind::Delete))
            .count() as u32
    }

    /// Number of post-image lines: context + insertions.
    pub fn new_count(&self) -> u32 {
        self.lines
            .iter()
            .filter(|l| matches!(l.kind, LineKind::Context | LineKind::Insert))
            .count() as u32
    }

    /// Insertions in this hunk.
    pub fn insertions(&self) -> usize {
        self.lines
            .iter()
            .filter(|l| l.kind == LineKind::Insert)
            .count()
    }

    /// Deletions in this hunk.
    pub fn deletions(&self) -> usize {
        self.lines
            .iter()
            .filter(|l| l.kind == LineKind::Delete)
            .count()
    }
}

/// One file's worth of diff.
///
/// `old_path` and `new_path` are equal for everything but a
/// [`FileChange::Renamed`]. Added and deleted files still carry a real path on
/// both fields — the `/dev/null` in the formatted output is *derived* from
/// [`change`](Self::change), matching git, which writes
/// `diff --git a/new b/new` even for a file it is creating.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileDiff {
    /// Pre-image path, without the `a/` prefix.
    pub old_path: String,
    /// Post-image path, without the `b/` prefix.
    pub new_path: String,
    /// What happened to the file.
    pub change: FileChange,
    /// Changed regions, in ascending pre-image order.
    pub hunks: Vec<Hunk>,
}

impl FileDiff {
    /// An empty modify entry for `path`. Mostly useful in tests.
    pub fn new(path: impl Into<String>) -> Self {
        let path = path.into();
        Self {
            old_path: path.clone(),
            new_path: path,
            change: FileChange::Modified,
            hunks: Vec::new(),
        }
    }

    /// Insertions across all hunks.
    pub fn insertions(&self) -> usize {
        self.hunks.iter().map(Hunk::insertions).sum()
    }

    /// Deletions across all hunks.
    pub fn deletions(&self) -> usize {
        self.hunks.iter().map(Hunk::deletions).sum()
    }
}

/// Why a model is incomplete, and by how much.
///
/// A model carrying this is **not a patch**. [`crate::format`] writes an
/// unmistakable marker line before the first file section so that the text
/// cannot be mistaken for a complete diff by a human, a model, or `git apply`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Truncation {
    /// Whole file sections dropped.
    pub omitted_files: usize,
    /// Hunks dropped from files that were otherwise kept.
    pub omitted_hunks: usize,
    /// Total diff lines dropped, across both of the above.
    pub omitted_lines: usize,
}

impl Truncation {
    /// True when nothing was actually dropped.
    pub fn is_empty(&self) -> bool {
        self.omitted_files == 0 && self.omitted_hunks == 0 && self.omitted_lines == 0
    }
}

/// A whole multi-file diff.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct DiffModel {
    /// File sections in emission order.
    pub files: Vec<FileDiff>,
    /// `Some` when this model is a truncated projection of a larger one.
    pub truncated: Option<Truncation>,
}

impl DiffModel {
    /// A complete model over `files`.
    pub fn new(files: Vec<FileDiff>) -> Self {
        Self {
            files,
            truncated: None,
        }
    }

    /// True when this model is a complete diff — safe to call a patch.
    pub fn is_complete(&self) -> bool {
        self.truncated.is_none()
    }

    /// Files changed, insertions, deletions.
    pub fn stat(&self) -> DiffStat {
        DiffStat {
            files_changed: self.files.len(),
            insertions: self.files.iter().map(FileDiff::insertions).sum(),
            deletions: self.files.iter().map(FileDiff::deletions).sum(),
        }
    }

    /// Total number of diff body lines (excluding headers) across all files.
    pub fn line_count(&self) -> usize {
        self.files
            .iter()
            .flat_map(|f| f.hunks.iter())
            .map(|h| h.lines.len())
            .sum()
    }
}
