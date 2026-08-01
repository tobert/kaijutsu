//! Typed errors for diff generation and unified-diff parsing.
//!
//! Every rejection path in this crate lands here. There is deliberately no
//! "lenient" mode and no variant that means "skipped something": a construct we
//! do not model is an error, because a diff that silently drops a file section
//! is worse than no diff at all — it reads as a complete, smaller change.

use thiserror::Error;

/// Which side of a hunk a count disagreement was found on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    /// The `-` (pre-image) side.
    Old,
    /// The `+` (post-image) side.
    New,
}

impl std::fmt::Display for Side {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Side::Old => f.write_str("old"),
            Side::New => f.write_str("new"),
        }
    }
}

/// Everything that can go wrong producing or reading a diff.
///
/// Line numbers are 1-based positions in the *input text* (after CRLF
/// normalization, which never changes the line count).
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
    #[error("line {line}: hunk declared {declared} {side}-side line(s) but the body held {actual}")]
    HunkCountMismatch {
        /// 1-based line number of the hunk header.
        line: usize,
        /// Which side disagreed.
        side: Side,
        /// The count from the `@@` header.
        declared: u32,
        /// The count actually present in the body.
        actual: u32,
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
    /// CRLF is normalized to `\n` on the way into the model, so a
    /// CRLF-vs-LF-only change would otherwise diff to *nothing* — a silent
    /// empty result for a real change. We refuse instead.
    #[error("{path}: the two versions differ only in line endings (CRLF is normalized to LF)")]
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
