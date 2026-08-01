//! Content-shaped diff profiles.
//!
//! imara-diff diffs *arbitrary token streams*, so "diff this like prose" and
//! "diff this like music" are tokenizer configuration, not new algorithms. A
//! [`DiffProfile`] names one such configuration.
//!
//! # Why this enum is not `ContentType`
//!
//! This crate deliberately does **not** depend on `kaijutsu-types`. The MIME
//! vocabulary grows on the kernel's schedule; the tokenizers grow on this
//! crate's. Callers map `ContentType` → [`DiffProfile`] at the edge, so a new
//! content type never touches the algorithm crate and a new profile never
//! forces a wire change.

/// How to tokenize and refine a particular kind of content.
///
/// `#[non_exhaustive]` on purpose: the deferred variants below are coming, and
/// downstream `match`es should be written to accommodate them from day one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum DiffProfile {
    /// Lines as the primary token, with word-level refinement inside changed
    /// line pairs. The right default for code and for plain text.
    ///
    /// This is the only profile v1 implements.
    #[default]
    LineWord,
    //
    // ── Deferred profiles ───────────────────────────────────────────────────
    //
    // These are documented rather than implemented so the seam is visible and
    // the reasons for waiting stay attached to it.
    //
    // `Paragraph` — markdown and prose. Line identity is *weak* in prose
    // because a rewrap changes every line without changing a word; a
    // paragraph-primary tokenizer (blank-line separated, word-refined) makes a
    // rewrap read as "nothing changed". Deferred because the interesting cases
    // are list/table/code-fence boundaries inside markdown, which want a real
    // corpus before we pick a splitter.
    //
    // `AbcBars` — ABC music notation. The bar line `|` is a natural token
    // boundary, so hunks would land on *measures* instead of arbitrary text
    // lines, and bar-aligned text hunks map 1:1 onto engraved measures — the
    // text diff becomes the index into a future notation diff. Deferred by
    // batch-review consensus: repeats, overlays, and multi-voice have enough
    // edge cases to deserve a corpus rather than a guess.
}

impl DiffProfile {
    /// Whether this profile performs word-level refinement inside changed
    /// regions.
    pub fn refines_words(&self) -> bool {
        match self {
            DiffProfile::LineWord => true,
        }
    }
}
