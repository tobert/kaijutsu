//! Paths to the golden fixtures — the shared dialect authority.
//!
//! The fixture files live in this crate (`crates/kaijutsu-diff/fixtures/`) so
//! that kernel and app tests reference the *same* bytes this crate's parser is
//! tested against. Two sides inventing divergent dialects is the failure mode
//! this module exists to prevent; add a dev-dependency on `kaijutsu-diff` and
//! read them through [`path`] rather than copying text into a test.
//!
//! Three directories:
//!
//! - `canonical/` — exactly what [`crate::format`] emits. `format(parse(t)) == t`.
//! - `external/` — valid input in the accepted dialect that is *not* canonical
//!   (git `index` lines, `diff -u` timestamps, `,1` counts spelled out).
//!   Canonicalization is idempotent on these, not identity.
//! - `invalid/` — must produce a [`crate::DiffError`]. Never a partial model.

use std::path::{Path, PathBuf};

/// The fixture directory, baked in at compile time.
pub const FIXTURE_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/fixtures");

/// The fixture directory as a [`PathBuf`].
pub fn dir() -> PathBuf {
    PathBuf::from(FIXTURE_DIR)
}

/// Resolve a fixture path such as `"canonical/multi_file.diff"`.
pub fn path(relative: impl AsRef<Path>) -> PathBuf {
    dir().join(relative)
}

/// Read a fixture, panicking with its path if it is missing.
pub fn read(relative: impl AsRef<Path>) -> String {
    let p = path(relative);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("reading fixture {}: {e}", p.display()))
}

/// Fixtures that are byte-for-byte canonical output.
pub const CANONICAL: &[&str] = &[
    "canonical/single_file_modify.diff",
    "canonical/multi_file.diff",
    "canonical/add_file.diff",
    "canonical/delete_file.diff",
    "canonical/rename_with_edits.diff",
    "canonical/rename_pure.diff",
    "canonical/no_newline.diff",
    "canonical/quoted_path.diff",
    "canonical/empty_context_line.diff",
    "canonical/section_heading.diff",
    "canonical/truncated.diff",
];

/// Valid, accepted, but not canonical — the canonicalization corpus.
pub const EXTERNAL: &[&str] = &[
    "external/git_index_headers.diff",
    "external/plain_diff_u.diff",
    "external/explicit_single_counts.diff",
    "external/git_octal_quoted_path.diff",
    "external/stripped_trailing_space.diff",
];

/// Fixtures that must be rejected, with the variant each must produce.
///
/// The variant is named as a string rather than a [`crate::DiffError`] value so
/// consumers in other crates can assert on it without matching on a type they
/// do not depend on.
pub const INVALID: &[(&str, &str)] = &[
    ("invalid/binary_git.diff", "BinaryPatch"),
    ("invalid/binary_files_differ.diff", "BinaryPatch"),
    ("invalid/malformed_hunk_header.diff", "MalformedHunkHeader"),
    ("invalid/hunk_count_mismatch.diff", "HunkCountMismatch"),
    ("invalid/unknown_extension.diff", "UnsupportedExtension"),
    ("invalid/copy_headers.diff", "UnsupportedExtension"),
    ("invalid/garbage_preamble.diff", "ExpectedFileHeader"),
    ("invalid/stray_no_newline.diff", "StrayNoNewline"),
    ("invalid/missing_post_image.diff", "MissingPostImageHeader"),
];
