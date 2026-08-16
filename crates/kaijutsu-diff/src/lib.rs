//! # kaijutsu-diff — diff model, dialect, and refinement
//!
//! A pure crate: no Bevy, no kernel, no RPC, no async, and deliberately **no
//! dependency on `kaijutsu-types`**. It knows how to turn two versions of a
//! file into a [`DiffModel`], how to write that model as unified-diff text,
//! how to read unified-diff text back, and how to cut a diff down to a budget
//! without pretending the result is whole. Everything else — where the two
//! versions came from, what MIME type the block carries, how the hunks are
//! drawn — lives in the crates that depend on this one.
//!
//! The diff algorithm is [`imara-diff`](https://docs.rs/imara-diff)'s. We write
//! none of our own.
//!
//! ```
//! use kaijutsu_diff::{diff_file, format, parse, DiffModel, DiffOptions, FileSpec};
//!
//! let file = diff_file(
//!     &FileSpec::modified("greet.rs", "fn main() {}\n", "fn main() { greet(); }\n"),
//!     &DiffOptions::default(),
//! )?;
//! let model = DiffModel::new(vec![file]);
//! assert_eq!(model.stat().to_string(), "1 file, +1 −1");
//!
//! let text = format(&model);
//! assert_eq!(parse(&text)?, model);
//! # Ok::<(), kaijutsu_diff::DiffError>(())
//! ```
//!
//! # The accepted dialect
//!
//! Pinned in [`parse`]'s module documentation, exercised by the golden
//! fixtures in [`fixtures`], and summarized here:
//!
//! - Multi-file patches; git-style `diff --git` sections *and* plain `diff -u`
//!   output.
//! - File creation and deletion via `/dev/null` on the `---`/`+++` headers.
//! - `\ No newline at end of file` markers, on either side, mid-hunk or trailing.
//! - Paths C-quoted git-style when they contain a space, a quote, a backslash,
//!   or a control character. Non-ASCII is **not** escaped.
//! - **Renames are represented, not detected.** `rename from` / `rename to` are
//!   parsed and re-emitted; nothing here performs similarity detection. That
//!   arrives with the git-backed sources.
//! - **Copies are not modelled at all** — `copy from` / `copy to` are rejected.
//! - Binary patches (`GIT binary patch`, `Binary files … differ`) are rejected
//!   as [`DiffError::BinaryPatch`], and every unrecognised construct as
//!   [`DiffError::UnsupportedExtension`] or [`DiffError::ExpectedFileHeader`].
//!   There is no lenient mode and nothing is ever silently skipped: a parser
//!   that drops a section produces a *smaller diff that still looks complete*,
//!   which is the most dangerous thing a diff viewer can do.
//! - Line terminators — `\r\n`, a bare `\r`, and `\n` alike — are normalized to
//!   `\n` **on ingest**, so no `\r` ever enters a model
//!   and emission is unconditionally LF. A change consisting only of line
//!   terminators is therefore not representable — [`diff_file`] returns
//!   [`DiffError::LineEndingsOnly`] rather than an empty diff.
//! - Canonicalization is lossy in exactly one direction, documented in
//!   [`parse`]: git `index` lines, file modes, and similarity percentages are
//!   accepted, interpreted, and dropped.
//!
//! # The three roundtrip properties
//!
//! These are the dialect's contract, and they are property-tested in
//! `tests/roundtrip.rs`:
//!
//! 1. `parse ∘ format == id` on models — formatting loses nothing a model holds.
//! 2. `format ∘ parse == id` on canonical text — parsing loses nothing canonical
//!    text holds.
//! 3. Arbitrary dialect-valid text **canonicalizes**: `format ∘ parse` is
//!    idempotent, so a diff that has been through this crate once is stable.
//!
//! Word spans are derived, not carried by the text; [`parse`] recomputes them
//! with the same refinement the generator used, which is what makes (1) hold.
//! [`model::FoldState`] is view state and is likewise not carried.
//!
//! # Truncation contract
//!
//! Diffs outgrow every budget: a render frame, a model's context window, a
//! preview row. [`truncate_to_bytes`] and [`truncate_to_lines`] are the only
//! sanctioned way to spend one, because they hold three lines that a naive
//! character-count truncation does not:
//!
//! - **Cuts land on whole-hunk boundaries.** A hunk cut in half keeps a
//!   plausible `@@` header whose counts no longer match its body — a patch that
//!   looks real and applies wrong.
//! - **The result is marked incomplete**, in the model
//!   ([`DiffModel::truncated`]) and in the text (a leading
//!   [`TRUNCATION_MARKER_PREFIX`] line, which no valid construct in the dialect
//!   can be confused for). A truncated diff must never look like a complete
//!   patch.
//! - **The canonical block keeps the full diff.** Truncation is a *projection*
//!   for one consumer, never a rewrite of the source of truth. In particular,
//!   hydration is a projection — diffstat plus whole-hunk-bounded content with
//!   an explicit complete/truncated marker — not a passthrough of the block.
//!
//! # Freeze-on-open viewer contract
//!
//! The viewer lives in `kaijutsu-app`, but the invariant it must hold is part
//! of this crate's contract because it is about the *model*, and because a
//! future side-by-side or minimap projection must hold it too:
//!
//! - A viewer binds to `(block_id, content version/hash)` **at open** and holds
//!   the [`DiffModel`] parsed from that exact content for its whole lifetime.
//! - If the underlying block changes while the viewer is open, it shows a stale
//!   banner with an explicit refresh. It **never silently rebinds**: line
//!   numbers, hunk indices, folds, and the cursor are all positions in the
//!   frozen model, and swapping the model underneath them moves the reader's
//!   place without telling them.
//! - Yank always reads the **frozen** model, never current block content.
//!   Copying a hunk that has since changed is a correctness bug you cannot see.
//! - Because content and content-type are independent fields, set separately
//!   and never validated against each other, a block can legitimately declare
//!   itself a diff while holding text that does not parse. Consumers must
//!   handle [`parse`] failing on
//!   declared-`Diff` content — loudly, as a visible error state, not as an
//!   empty viewer.
//!
//! # Profiles
//!
//! [`DiffProfile`] names a tokenizer/refinement configuration. v1 implements
//! [`DiffProfile::LineWord`] only; the deferred paragraph-aware and ABC
//! bar-aligned profiles are documented on the enum so the seam stays visible.
//! Callers map their own content types onto a profile at the edge — that is
//! why this crate does not know what a `ContentType` is.

#[cfg(feature = "viewer")]
pub mod core;
pub mod error;
pub mod fixtures;
pub mod limits;
pub mod minimap;
pub mod model;
pub mod paths;
pub mod profile;
pub mod stat;

mod engine;
mod format;
mod parse;
mod refine;
mod tokenize;
mod truncate;

#[cfg(feature = "viewer")]
pub use core::{
    CharSpan, DiffCore, DiffIntent, DiffRow, FoldSummary, RowKind, YankKind, visible_row_indices,
};
pub use engine::{DiffAlgorithm, DiffOptions, FileSpec, diff, diff_file};
pub use error::DiffError;
pub use minimap::{MinimapBucket, MinimapClass, minimap_buckets};
pub use format::{TRUNCATION_MARKER_PREFIX, format, format_file, format_hunk, truncation_marker};
pub use model::{
    DiffLine, DiffModel, FileChange, FileDiff, FoldState, Hunk, LineKind, Truncation, WordSpan,
};
pub use parse::{parse, parse_with};
pub use profile::DiffProfile;
pub use refine::refine_hunk;
pub use stat::DiffStat;
pub use tokenize::{Words, normalize_newlines};
pub use truncate::{truncate_to_bytes, truncate_to_lines};
