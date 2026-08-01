//! Size ceilings — the named tunables every stage of the diff pipeline checks.
//!
//! These are **policy, not physics**. They exist so that a pathological input
//! fails loudly at a named boundary instead of melting a render thread or
//! quietly eating a model's context window. Every one of them is a `const` so a
//! caller can reference the same number in a log line, a test, or an error
//! message rather than re-deriving it.
//!
//! Three stages, three budgets:
//!
//! | Stage        | Constant                       | Who enforces it                     |
//! |--------------|--------------------------------|-------------------------------------|
//! | generation   | [`MAX_INPUT_FILE_BYTES`]       | [`crate::diff_file`]                |
//! | render       | [`MAX_RENDER_BYTES`]           | the app, via [`crate::truncate_to_bytes`] |
//! | hydration    | [`MAX_HYDRATION_BYTES`]        | the kernel, via [`crate::truncate_to_bytes`] |
//!
//! The render and hydration ceilings are deliberately *not* enforced inside
//! this crate: only the caller knows which budget applies. What this crate
//! guarantees is that spending a budget goes through [`crate::truncate_to_bytes`],
//! which cuts at whole-hunk boundaries and marks the result incomplete.

/// Largest single-file content (per side) [`crate::diff_file`] will diff.
///
/// Exceeding it is [`crate::DiffError::TooLarge`], never a partial diff — a
/// half-diffed file is indistinguishable from a small change.
pub const MAX_INPUT_FILE_BYTES: usize = 8 * 1024 * 1024;

/// Largest unified-diff text [`crate::parse`] will accept.
///
/// Parsing is O(n) but allocates a `DiffLine` per line; this bounds the
/// allocation a hostile or runaway producer can force.
pub const MAX_PARSE_BYTES: usize = 16 * 1024 * 1024;

/// Ceiling on a generated diff before the producer should truncate.
///
/// Advisory: the generator does not enforce it (a large-but-honest diff is
/// still correct). `kj diff` and the `diff_block` MCP tool are expected to
/// compare [`crate::format`] output against this and truncate rather than
/// store an unbounded block.
pub const MAX_GENERATED_BYTES: usize = 4 * 1024 * 1024;

/// Ceiling on the diff text a client should lay out and render at once.
///
/// Parley layout is linear in glyphs but a multi-megabyte diff will still
/// stall a frame. The viewer truncates to this and shows the incomplete
/// marker; the canonical block keeps the full text.
pub const MAX_RENDER_BYTES: usize = 1024 * 1024;

/// Ceiling on the diff text placed in a model-facing hydration envelope.
///
/// This is the *token economics* budget the batch reviewers called the design's
/// top product risk. Hydration is a projection, never a passthrough: the
/// canonical block keeps the whole diff, the envelope gets a diffstat plus
/// whole-hunk-bounded content and an explicit complete/truncated marker.
pub const MAX_HYDRATION_BYTES: usize = 32 * 1024;

/// Largest changed region (removed bytes + added bytes) that gets word-level
/// refinement.
///
/// Word refinement is a second diff over a much finer token stream. On a
/// wholesale file rewrite it is both expensive and useless — every word is
/// "changed". Above this ceiling the region keeps line granularity and its
/// [`crate::DiffLine::words`] stay empty, which renders as a plain
/// added/removed line.
pub const MAX_REFINE_REGION_BYTES: usize = 64 * 1024;

/// Default number of unchanged context lines around each hunk.
///
/// Three, because that is what `diff -u` and `git diff` default to and the
/// dialect is worth nothing if it surprises the tools around it.
pub const DEFAULT_CONTEXT_LINES: usize = 3;

/// Unchanged gap (in words) that semantic cleanup will swallow to keep an
/// intra-line highlight word-shaped instead of letter-shaped.
///
/// Without this, `foo_bar` → `foo_baz` highlights three fragments around the
/// shared letters. See [`crate::refine`].
pub const SEMANTIC_MERGE_GAP_BYTES: usize = 4;

/// Default number of diff lines an inline conversation preview shows before
/// the reader expands into the full viewer.
pub const DEFAULT_PREVIEW_LINES: usize = 20;
