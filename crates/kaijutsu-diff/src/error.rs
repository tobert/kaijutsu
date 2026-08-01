//! Typed errors for diff generation and unified-diff parsing.
//!
//! Every rejection path in this crate lands here. There is deliberately no
//! "lenient" mode and no variant that means "skipped something": a construct we
//! do not model is an error, because a diff that silently drops a file section
//! is worse than no diff at all — it reads as a complete, smaller change.

use thiserror::Error;

/// Everything that can go wrong producing or reading a diff.
///
/// Line numbers are 1-based positions in the input text **as normalized** (see
/// [`crate::normalize_newlines`]). For `\r\n` and `\n` input that is the same
/// as the raw position; input containing a bare `\r` gains a line break there,
/// so the two can differ — normalized positions are the useful ones, since
/// they match what the model holds.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum DiffError {
    /// Top-level input did not start a file section where one was required.
    #[error("line {line}: expected a file header (`diff --git ` or `--- `), found {found:?}")]
    ExpectedFileHeader {
        /// 1-based line number.
        line: usize,
        /// The offending line, verbatim.
        found: String,
    },

    /// A `--- ` header was not followed by the matching `+++ ` header.
    #[error("line {line}: `--- ` header not followed by a `+++ ` header, found {found:?}")]
    MissingPostImageHeader {
        /// 1-based line number.
        line: usize,
        /// The offending line, verbatim.
        found: String,
    },

    /// A file section carried neither `---`/`+++` headers nor rename headers,
    /// so its paths are unknowable. The `diff --git` line is advisory only —
    /// its paths are ambiguous when they contain spaces.
    #[error(
        "line {line}: file section has no authoritative paths (no `---`/`+++`, no rename headers)"
    )]
    MissingPaths {
        /// 1-based line number of the `diff --git` line.
        line: usize,
    },

    /// Header lines disagreed about what happened to the file — e.g. a
    /// `deleted file mode` beside a `+++ b/path` post-image.
    #[error("line {line}: conflicting file headers: {detail}")]
    ConflictingFileHeaders {
        /// 1-based line number.
        line: usize,
        /// What disagreed with what.
        detail: String,
    },

    /// A `@@ ... @@` line did not parse.
    #[error("line {line}: malformed hunk header: {found:?}")]
    MalformedHunkHeader {
        /// 1-based line number.
        line: usize,
        /// The offending line, verbatim.
        found: String,
    },

    /// A hunk header promised more (or fewer) lines than the hunk body held.
    ///
    /// Both sides are always reported. A context line belongs to both, so a
    /// single stray one overruns both counts — naming only one side would tell
    /// half the truth about where the patch went wrong.
    #[error(
        "line {line}: hunk declared {declared_old} old / {declared_new} new line(s) \
         but the body held {actual_old} / {actual_new}"
    )]
    HunkCountMismatch {
        /// 1-based line number of the hunk header.
        line: usize,
        /// Pre-image count from the `@@` header.
        declared_old: u32,
        /// Pre-image lines actually present in the body.
        actual_old: u32,
        /// Post-image count from the `@@` header.
        declared_new: u32,
        /// Post-image lines actually present in the body.
        actual_new: u32,
    },

    /// A line inside a hunk body started with something other than
    /// ` `, `-`, `+`, or `\`.
    #[error("line {line}: unexpected line inside a hunk body: {found:?}")]
    UnexpectedHunkLine {
        /// 1-based line number.
        line: usize,
        /// The offending line, verbatim.
        found: String,
    },

    /// A `\ No newline at end of file` marker with no content line to attach to.
    #[error("line {line}: `\\ No newline at end of file` with no preceding content line")]
    StrayNoNewline {
        /// 1-based line number.
        line: usize,
    },

    /// A binary patch. Rejected loudly and on purpose — this crate models text.
    #[error("line {line}: binary patches are not supported: {found:?}")]
    BinaryPatch {
        /// 1-based line number.
        line: usize,
        /// The offending line, verbatim.
        found: String,
    },

    /// A git extended header we recognise the shape of but deliberately do not
    /// model (today: `copy from` / `copy to`), or one we do not recognise at all.
    #[error("line {line}: unsupported extended header: {found:?}")]
    UnsupportedExtension {
        /// 1-based line number.
        line: usize,
        /// The offending line, verbatim.
        found: String,
    },

    /// A double-quoted path whose C-style escapes did not decode.
    #[error("line {line}: malformed quoted path: {found:?}")]
    MalformedPath {
        /// 1-based line number.
        line: usize,
        /// The offending path token, verbatim.
        found: String,
    },

    /// The truncation marker line was present but did not parse.
    #[error("line {line}: malformed truncation marker: {found:?}")]
    MalformedTruncationMarker {
        /// 1-based line number.
        line: usize,
        /// The offending line, verbatim.
        found: String,
    },

    /// Input exceeded a named ceiling from [`crate::limits`].
    #[error("{what} is {actual} bytes, over the {limit}-byte ceiling")]
    TooLarge {
        /// Human-readable name of what was measured.
        what: &'static str,
        /// The ceiling that was exceeded.
        limit: usize,
        /// The measured size.
        actual: usize,
    },

    /// The two sides differ only in line terminators.
    ///
    /// `\r\n` and bare `\r` are both normalized to `\n` on the way into the
    /// model, so a terminator-only change (CRLF→LF, CR→LF, CRLF→CR …) would
    /// otherwise diff to *nothing* — a silent empty result for a real change.
    /// We refuse instead.
    #[error("{path}: the two versions differ only in line endings (all are normalized to LF)")]
    LineEndingsOnly {
        /// The path that was being diffed.
        path: String,
    },
}

impl DiffError {
    /// The variant's name, for tests that assert on a rejection without
    /// matching the whole struct.
    ///
    /// [`crate::fixtures::INVALID`] pairs each rejected fixture with one of
    /// these strings, so a consumer in another crate can assert on the right
    /// rejection without depending on this enum's shape.
    pub fn variant_name(&self) -> &'static str {
        match self {
            DiffError::ExpectedFileHeader { .. } => "ExpectedFileHeader",
            DiffError::MissingPostImageHeader { .. } => "MissingPostImageHeader",
            DiffError::MissingPaths { .. } => "MissingPaths",
            DiffError::ConflictingFileHeaders { .. } => "ConflictingFileHeaders",
            DiffError::MalformedHunkHeader { .. } => "MalformedHunkHeader",
            DiffError::HunkCountMismatch { .. } => "HunkCountMismatch",
            DiffError::UnexpectedHunkLine { .. } => "UnexpectedHunkLine",
            DiffError::StrayNoNewline { .. } => "StrayNoNewline",
            DiffError::BinaryPatch { .. } => "BinaryPatch",
            DiffError::UnsupportedExtension { .. } => "UnsupportedExtension",
            DiffError::MalformedPath { .. } => "MalformedPath",
            DiffError::MalformedTruncationMarker { .. } => "MalformedTruncationMarker",
            DiffError::TooLarge { .. } => "TooLarge",
            DiffError::LineEndingsOnly { .. } => "LineEndingsOnly",
        }
    }
}
