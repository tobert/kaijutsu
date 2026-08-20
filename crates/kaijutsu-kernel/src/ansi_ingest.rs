//! The `ansi-strip` ingest policy — one decision, six call sites.
//!
//! Terminal output reaches kaijutsu through a handful of narrow doors
//! (interactive shell stdout, `kj` capture, model tool results, rc/hook script
//! traces, background processes). Every one of them used to write whatever
//! bytes arrived straight into a block. `docs/ansi-and-beyond.md` says what
//! should happen instead: **strip to a projection, keep the original**.
//!
//! ```text
//! raw bytes ──> project() ──┬─ None  : no escape bytes, write the text as-is
//!                           └─ Some : clean text goes where the raw text went,
//!                                     then record() lands spans + provenance
//! ```
//!
//! Keeping the policy in one function is the point. The rules below are subtle
//! enough that six independent copies would drift within a month. One site —
//! `background_exec::AnsiDrain` — genuinely can't call [`project`] (it needs
//! the incremental parser for chunk-boundary state), so it shares the no-op
//! predicate ([`is_noop_projection`]) instead: one policy, two shapes.
//!
//! - **The fast path must stay free.** The overwhelming majority of shell
//!   output has no `ESC` in it at all. [`project`] answers that case with a
//!   single `memchr` (`<[u8]>::contains` specializes to `memchr` for `u8`) and
//!   allocates nothing, sets no tag, writes no provenance row. A block with no
//!   escape bytes is byte-identical to what it was before this feature.
//! - **A tag whose original equals its content teaches nobody anything.**
//!   Escape bytes present but the projection byte-identical (a lone `ESC`
//!   that survived to `finish` with no other effect) gets the no-op
//!   treatment too — [`is_noop_projection`], shared by both shapes.
//! - **A tagged block always has a row.** Hook sites call [`record`], which
//!   writes the provenance row *before* the tag, so a crash in between leaves
//!   an orphan row (invisible, harmless) rather than a tag pointing at bytes
//!   nobody kept. `journal_op` is not transactional today
//!   (docs/ansi-and-beyond.md "Reality checks"), so this ordering is the
//!   guarantee, not a transaction.
//! - **Spans land after the text.** `BlockContent::edit_text` *clears*
//!   `style_spans` (char-addressed edit vs byte-addressed spans — see
//!   `blocks/content.rs`), so a hook that set spans before writing the text
//!   would silently lose them. End-appends keep spans, but every site here
//!   still orders text-then-spans so the rule needs no per-site reasoning.
//!
//! The transform never touches `edit_text_as`/`append_text_as` in general —
//! LLM token streams flow through those, and stripping model prose would be a
//! bug. Hooks are per-site, always.

use std::borrow::Cow;

use kaijutsu_ansi::{PARSER_VERSION, TRANSFORM_NAME, StyleSpan};
use kaijutsu_types::{BlockId, ContextId};

use crate::block_store::BlockStore;

/// The ESC byte that starts every sequence this transform recognizes.
pub(crate) const ESC: u8 = 0x1b;

/// A block's ANSI projection: the clean text that becomes block content, and
/// the spans describing how it was styled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnsiProjection {
    /// Stripped text — this is what gets written where the raw text used to.
    pub text: String,
    /// Styled spans, byte-addressed into [`Self::text`]. May be empty: output
    /// carrying only cursor motion or OSC still projects (the text changed),
    /// it just has no styling.
    pub spans: Vec<StyleSpan>,
}

/// Decide whether `raw` needs the ANSI transform, and run it if so.
///
/// `None` means **do nothing at all**: write the bytes exactly where they
/// would have gone, set no spans, write no provenance row, leave the block
/// tag-free. That is the common case and it costs one `memchr`.
///
/// `Some` means the block is a projection of something else, and the caller
/// owes it a [`record`] call after the clean text is in place.
///
/// The second guard — escape bytes present, but the projection is
/// byte-identical with no spans — is the pathological leftover (a lone ESC
/// that survived into the text). It gets the no-op treatment too: a tag whose
/// original equals its content teaches nobody anything.
pub fn project(raw: &[u8]) -> Option<AnsiProjection> {
    if !raw.contains(&ESC) {
        return None;
    }
    let (text, spans) = kaijutsu_ansi::strip(raw);
    if is_noop_projection(&text, &spans, raw) {
        return None;
    }
    Some(AnsiProjection { text, spans })
}

/// The second guard [`project`] applies, exported so the one streaming
/// site (`background_exec::AnsiDrain::finish`) can apply the same rule
/// instead of re-deriving it. True when a projection is a no-op worth
/// suppressing: no styling, and the projected text is byte-identical to what
/// was fed in — a tag whose original equals its content teaches nobody
/// anything. This is what keeps a stray lone `ESC` (with no other escape
/// bytes around it) from earning a tag, a `block_provenance` row, and a
/// `SpansChanged` event for a block nothing actually happened to.
pub(crate) fn is_noop_projection(text: &str, spans: &[StyleSpan], raw: &[u8]) -> bool {
    spans.is_empty() && text.as_bytes() == raw
}

/// Land the projection on a block that **already holds the clean text**.
///
/// Order is load-bearing twice over:
///
/// 1. Callers must have written the clean text first — `edit_text` clears
///    `style_spans`, so spans set before the text write would vanish.
/// 2. The provenance row is written before the tag, so the durable state can
///    never claim an original that was not stored.
///
/// Failures are logged, not propagated: a lost span map degrades a block to
/// unstyled-but-correct, and no ingest path should fail a command because the
/// styling metadata could not be recorded. The content is already durable by
/// the time this runs.
///
/// On a store with no db (replica stores, most kernel unit tests)
/// [`BlockStore::store_provenance`] no-ops and the block still gets its spans
/// and its tag. That is a tag with no row — exactly the gap
/// docs/ansi-and-beyond.md describes: `kj block original` reports it instead of
/// inventing bytes, and the CI invariant skips with a warning. Spans are still
/// worth having, so this does not suppress them.
pub fn record(
    blocks: &BlockStore,
    context_id: ContextId,
    block_id: &BlockId,
    spans: Vec<StyleSpan>,
    original: &[u8],
) {
    if let Err(e) = blocks.store_provenance(block_id, TRANSFORM_NAME, PARSER_VERSION, original) {
        // No row means no tag: `kj block reproject` would have nothing to
        // reproject from and the CI invariant would report a phantom gap.
        tracing::warn!(
            error = %e,
            block = %block_id,
            "ansi-strip: could not store provenance; leaving the block untagged and unstyled",
        );
        return;
    }
    if let Err(e) = blocks.set_style_spans(context_id, block_id, spans, Some(kaijutsu_ansi::provenance_tag()))
    {
        tracing::warn!(
            error = %e,
            block = %block_id,
            "ansi-strip: could not set style spans; the provenance row is an orphan (harmless)",
        );
    }
}

/// The raw stdout bytes of a kaish result, without a lossy detour.
///
/// [`kaish_kernel::interpreter::ExecResult::text_out`] is the wrong source for
/// provenance: on the `Bytes` arm it decodes with `U+FFFD` replacement, so the
/// "original" we stored would already be a projection. `out_bytes()` hands
/// back the binary arm verbatim; the text arm (and `text_out`'s fallback to
/// structured output's canonical string) is already a `String`, so borrowing
/// its bytes is lossless.
///
/// Note what this *cannot* recover: kaish applies its own output limiter
/// upstream (head+tail spill, 8 KB on the Agent profile), so foreground
/// provenance is post-cap bytes. "What kaish handed us" is the honest original
/// for these sites; `background_exec` streams from the pipe itself and can
/// capture pre-cap.
pub fn raw_stdout(result: &kaish_kernel::interpreter::ExecResult) -> Cow<'_, [u8]> {
    match result.out_bytes() {
        Some(bytes) => Cow::Borrowed(bytes),
        None => match result.text_out() {
            Cow::Borrowed(s) => Cow::Borrowed(s.as_bytes()),
            Cow::Owned(s) => Cow::Owned(s.into_bytes()),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escape_free_input_is_the_free_path() {
        assert_eq!(project(b"plain output\nwith lines\n"), None);
        assert_eq!(project(b""), None);
        // Other C0 controls do NOT trigger the transform: the predicate is
        // "escape bytes", one memchr, and a BEL is not worth a parser run.
        assert_eq!(project(b"bell\x07here"), None);
    }

    #[test]
    fn sgr_input_projects_to_clean_text_and_spans() {
        let raw = b"a\x1b[31mred\x1b[0mb";
        let p = project(raw).expect("escape bytes must project");
        assert_eq!(p.text, "aredb");
        assert_eq!(p.spans.len(), 1);
        // The CI invariant, in miniature: the projection is exactly what the
        // transform yields for the bytes we would store as provenance.
        assert_eq!(kaijutsu_ansi::strip(raw), (p.text.clone(), p.spans.clone()));
    }

    #[test]
    fn escape_sequences_without_styling_still_project() {
        // Cursor motion: preserve-don't-render. No spans, but the text
        // changed, so the block genuinely is a projection and earns a tag.
        let p = project(b"before\x1b[2Jafter").expect("text changed");
        assert_eq!(p.text, "beforeafter");
        assert!(p.spans.is_empty());
    }
}
