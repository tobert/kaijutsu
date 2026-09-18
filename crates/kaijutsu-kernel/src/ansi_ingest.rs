//! Project ANSI output into clean text while retaining its original bytes.
//!
//! `project` returns `None` without allocation when no transform is needed.
//! Command, model-tool, and rc settlement commit clean text, spans, provenance,
//! and related execution state in one block-journal transaction. A failed
//! transaction leaves the prior result intact and reports the storage error.
//!
//! General text edits and model token streams are not ANSI ingest sites.
//! See `docs/ansi-and-beyond.md`.

use std::borrow::Cow;

use kaijutsu_ansi::StyleSpan;
#[cfg(test)]
use kaijutsu_ansi::{PARSER_VERSION, TRANSFORM_NAME};
#[cfg(test)]
use kaijutsu_types::{BlockId, ContextId};

#[cfg(test)]
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
/// `Some` means the block is a projection of something else. Settlement
/// commits its text, spans, original bytes, and related terminal state
/// together.
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

/// A projection is a no-op when it changes neither styling nor text.
pub(crate) fn is_noop_projection(text: &str, spans: &[StyleSpan], raw: &[u8]) -> bool {
    spans.is_empty() && text.as_bytes() == raw
}

/// Test helper for landing a projection on an existing clean-text block.
/// Production writers use recorded BlockStore acceptance so content, spans,
/// provenance, and settlement state commit together.
///
/// Order is load-bearing twice over:
///
/// 1. Callers must have written the clean text first — `edit_text` clears
///    `style_spans`, so spans set before the text write would vanish.
/// 2. The provenance row is written before the tag, so the durable state can
///    never claim an original that was not stored.
///
/// This helper preserves the older two-step behavior so provenance fixtures
/// can exercise missing rows and partial metadata. Production settlement must
/// propagate any storage failure from its atomic transaction.
#[cfg(test)]
pub fn record(
    blocks: &BlockStore,
    context_id: ContextId,
    block_id: &BlockId,
    spans: Vec<StyleSpan>,
    original: &[u8],
) {
    if let Err(e) = blocks.store_provenance(block_id, TRANSFORM_NAME, PARSER_VERSION, original) {
        tracing::warn!(error = %e, block = %block_id,
            "ansi-strip: could not store provenance; leaving the block untagged and unstyled");
        return;
    }
    let tag = kaijutsu_ansi::provenance_tag();
    if let Err(e) = blocks.set_style_spans(context_id, block_id, spans, Some(tag)) {
        tracing::warn!(error = %e, block = %block_id,
            "ansi-strip: could not set style spans; the provenance row is an orphan (harmless)");
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
/// for these sites.
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
