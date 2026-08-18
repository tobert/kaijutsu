//! Splitting a formatted block into shapeable chunks.
//!
//! A chunk is a run of **complete hard lines**. That is the whole trick: a
//! chunk boundary only ever lands immediately after a `\n`, and parley never
//! wraps a line across a hard break, so shaping the chunks separately and
//! stacking their layouts reproduces the whole block's line grid exactly.
//! (Brushes are not a shaping property either, which is what
//! `layout_bridge::spanned_and_plain_layouts_position_glyphs_identically`
//! already pins; `chunked_and_whole_layouts_position_glyphs_identically`
//! below is the same claim for hard-line splits.)
//!
//! # The one place chunking is not bit-exact: baseline quantization
//!
//! `VelloFont::layout` builds with parley's `quantize` flag on
//! (`text/shaping/font.rs`), so parley **rounds each line's baseline to a
//! whole logical pixel** while the underlying line height stays fractional
//! (21.792 px for the shipped mono at 16 px, baselines 16 / 38 / 60 / 81).
//! The rounding phase restarts at every layout, so a chunk stacked at a
//! fractional offset lands its baselines up to one pixel away from where the
//! whole-block layout put them. What *is* exact:
//!
//! - **Chunk heights sum to the whole layout's height, to the bit** — the
//!   line grid, the block height, and every geometry offset derived from it
//!   are identical whether or not the block was chunked.
//! - **Every glyph's x is identical**, and every glyph is the same glyph.
//! - The y difference is bounded below 1 logical px and **does not
//!   accumulate**: each chunk re-derives its baselines from its own top, so
//!   the error is two roundings, not N.
//!
//! Both facts are pinned by the tests below. Sub-pixel is the right place for
//! this to land — the surface renderer snaps baselines to *physical* pixels
//! at draw time, which is where snapping is meaningful on a fractional-DPI
//! display anyway (see `layout_bridge`'s note on why the collectors leave
//! baselines raw).
//!
//! Chunking buys two things the rewrite needs: a streamed block's tail
//! re-shapes at chunk granularity instead of whole-block — see
//! [`frozen_chunk_count`] and `shape_cache::incremental_prefix` — and a giant
//! block's shaped glyphs live in several right-sized `Arc`s that the extractor
//! can hand to the GPU without ever deep-cloning the whole thing.

use std::ops::Range;

use crate::text::rich::SpanBrush;

/// Target hard lines per chunk.
///
/// Big enough that an ordinary block is one chunk (no per-chunk overhead in
/// the common case), small enough that the re-shape cost of a streaming tail
/// is bounded regardless of how tall the block grows.
pub const CHUNK_LINES: usize = 64;

/// Byte ranges tiling `text`, each holding at most `chunk_lines` complete
/// hard lines.
///
/// Every boundary sits immediately after a `\n`. The ranges tile the text
/// exactly — `ranges[0].start == 0`, `ranges[i].end == ranges[i+1].start`,
/// `ranges.last().end == text.len()` — with no gaps and no overlap, so a
/// caller can slice spans, offsets or edits against them without a
/// reconciliation pass. The final chunk is whatever is left over and
/// therefore commonly lacks a trailing newline (formatted block text is
/// `trim_end`ed, so it usually has none at all).
///
/// **Empty text yields one empty range** (`vec![0..0]`) rather than no
/// ranges. That keeps "shape every chunk and stack the heights" exactly
/// equivalent to "shape the whole text": an empty block still gets one
/// layout, and whatever height parley gives an empty string is the height
/// the row measures — rather than a silent zero that would disagree with the
/// legacy path.
pub fn chunk_ranges(text: &str, chunk_lines: usize) -> Vec<Range<usize>> {
    let chunk_lines = chunk_lines.max(1);
    if text.is_empty() {
        return vec![0..0];
    }

    let mut ranges = Vec::new();
    let mut start = 0usize;
    let mut lines = 0usize;
    for (i, byte) in text.bytes().enumerate() {
        if byte != b'\n' {
            continue;
        }
        lines += 1;
        if lines == chunk_lines {
            ranges.push(start..i + 1);
            start = i + 1;
            lines = 0;
        }
    }
    // Trailing partial chunk. When the text ends exactly on a boundary
    // (`start == text.len()`) there is nothing left, and appending an empty
    // range here would shape an extra blank line onto the end of the block.
    if start < text.len() {
        ranges.push(start..text.len());
    }
    ranges
}

/// How many **leading** chunks of `ranges` are complete — full at
/// `chunk_lines` hard lines and closed by the `\n` that ended the last of
/// them.
///
/// This is the freeze predicate (slice 3): a complete chunk's bytes can never
/// change while text is appended to the end of the block, because
/// [`chunk_ranges`] is a left-to-right fold that only ever emits a boundary
/// once it has counted `chunk_lines` newlines. So a complete chunk's shaped
/// glyphs stay valid for the life of the stream and its `Arc` is reused by
/// pointer; only the trailing incomplete chunk re-shapes on a content bump.
///
/// [`chunk_ranges`] guarantees every range but the last is complete by
/// construction, so only the last one is actually examined — the check is O(one
/// chunk), not O(text). The last range is complete exactly when the text ended
/// on a boundary, which the fold expresses as "there was no remainder", but
/// `ranges` alone can't say that, hence the newline count.
pub fn frozen_chunk_count(text: &str, ranges: &[Range<usize>], chunk_lines: usize) -> usize {
    let chunk_lines = chunk_lines.max(1);
    let Some(last) = ranges.last() else {
        return 0;
    };
    let tail = &text[last.clone()];
    let complete =
        tail.ends_with('\n') && tail.bytes().filter(|b| *b == b'\n').count() == chunk_lines;
    if complete {
        ranges.len()
    } else {
        ranges.len() - 1
    }
}

/// The text a chunk is actually shaped from: its bytes minus **one** trailing
/// hard break.
///
/// A chunk that ends on a boundary ends with the `\n` that closed its last
/// line. Handed to parley as-is, that trailing break is an invitation to
/// emit an extra (empty) final line — which would add a line height to every
/// interior chunk and make the stacked layout drift away from the whole-text
/// one by a line per chunk. The break carries no glyphs, so dropping it
/// changes nothing else; the byte range keeps it, because ranges must tile.
pub fn chunk_shaping_text<'a>(text: &'a str, range: &Range<usize>) -> &'a str {
    let slice = &text[range.clone()];
    slice.strip_suffix('\n').unwrap_or(slice)
}

/// Clamp and rebase span brushes onto one chunk's local byte space.
///
/// Spans are authored against the whole block's text (that's what
/// `build_span_brushes` produces from markdown), while each chunk is shaped
/// as its own string starting at byte 0. A span crossing a chunk boundary
/// therefore appears in both chunks, clipped to each. Spans that miss the
/// range entirely — or that clip to nothing — are dropped: a zero-width
/// brush range colors no glyph, and parley's ranged builder has no use for
/// it.
pub fn slice_spans(spans: &[SpanBrush], range: Range<usize>) -> Vec<SpanBrush> {
    spans
        .iter()
        .filter_map(|span| {
            let start = span.start.max(range.start);
            let end = span.end.min(range.end);
            if start >= end {
                return None;
            }
            Some(SpanBrush {
                start: start - range.start,
                end: end - range.start,
                brush: span.brush.clone(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::text::msdf::layout_bridge::collect_msdf_glyphs_deferred;
    use crate::text::msdf::PositionedGlyph;
    use crate::text::shaping::{
        VelloFont, VelloFontAxes, VelloTextAlign, VelloTextStyle, load_into_font_context,
    };
    use peniko::{Brush, Color};

    fn brush(rgba: [u8; 4]) -> Brush {
        Brush::Solid(Color::from_rgba8(rgba[0], rgba[1], rgba[2], rgba[3]))
    }

    const WHITE: [u8; 4] = [255, 255, 255, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];

    /// The shipped mono font, registered into the shared collection exactly
    /// as the asset loader does — the same helper the layout-bridge tests
    /// use. Shaping for real is the point: the property under test is
    /// *parley's* line breaking, not our arithmetic.
    fn mono() -> VelloFont {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/fonts/NotoMono-Regular.ttf"
        ))
        .expect("shipped test font must be present");
        load_into_font_context(bytes)
    }

    /// The block-text style `shape_cache` builds — same font size and weight
    /// axis `build_block_scenes` uses, so the equivalence is pinned for the
    /// styling that actually ships.
    fn style() -> VelloTextStyle {
        VelloTextStyle {
            brush: brush(WHITE),
            font_size: 16.0,
            font_axes: VelloFontAxes {
                weight: Some(200.0),
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn span(start: usize, end: usize, rgba: [u8; 4]) -> SpanBrush {
        SpanBrush {
            start,
            end,
            brush: brush(rgba),
        }
    }

    // ---- chunk_ranges ----------------------------------------------------

    #[test]
    fn chunk_ranges_of_empty_text_is_one_empty_range() {
        assert_eq!(chunk_ranges("", 64), vec![0..0]);
    }

    #[test]
    fn short_text_is_a_single_chunk() {
        let text = "one\ntwo\nthree";
        assert_eq!(chunk_ranges(text, 64), vec![0..text.len()]);
    }

    #[test]
    fn boundaries_land_only_after_newlines() {
        let text = "a\nbb\nccc\ndddd\neeeee\n";
        for range in chunk_ranges(text, 2) {
            if range.start > 0 {
                assert_eq!(
                    &text[range.start - 1..range.start],
                    "\n",
                    "chunk starting at {} does not follow a hard break",
                    range.start,
                );
            }
        }
    }

    #[test]
    fn ranges_tile_the_text_exactly() {
        let text = "a\nbb\nccc\ndddd\neeeee\nffffff";
        let ranges = chunk_ranges(text, 2);
        assert_eq!(ranges.first().unwrap().start, 0);
        assert_eq!(ranges.last().unwrap().end, text.len());
        for pair in ranges.windows(2) {
            assert_eq!(pair[0].end, pair[1].start, "gap or overlap in {ranges:?}");
        }
        // Concatenating the slices reproduces the source byte for byte.
        let rejoined: String = ranges.iter().map(|r| &text[r.clone()]).collect();
        assert_eq!(rejoined, text);
    }

    #[test]
    fn text_ending_on_a_boundary_gets_no_trailing_empty_chunk() {
        // Four lines, chunked two at a time: the second chunk consumes the
        // final newline, so there is nothing left over.
        let text = "a\nb\nc\nd\n";
        let ranges = chunk_ranges(text, 2);
        assert_eq!(ranges, vec![0..4, 4..8]);
    }

    #[test]
    fn a_partial_final_chunk_keeps_the_remainder() {
        let text = "a\nb\nc";
        assert_eq!(chunk_ranges(text, 2), vec![0..4, 4..5]);
    }

    #[test]
    fn chunk_lines_zero_is_clamped_to_one_line_per_chunk() {
        let text = "a\nb\nc";
        assert_eq!(chunk_ranges(text, 0), chunk_ranges(text, 1));
        assert_eq!(chunk_ranges(text, 1).len(), 3);
    }

    #[test]
    fn text_with_no_newline_is_one_chunk_however_small_the_budget() {
        let text = "a single very long line that wraps a lot when laid out";
        assert_eq!(chunk_ranges(text, 1), vec![0..text.len()]);
    }

    #[test]
    fn chunk_shaping_text_drops_exactly_one_trailing_break() {
        let text = "a\nb\n\n";
        // Whole text as one chunk: only the last `\n` comes off, the blank
        // line before it survives (it is real content, not a boundary).
        assert_eq!(chunk_shaping_text(text, &(0..text.len())), "a\nb\n");
        // An interior chunk loses its boundary newline.
        assert_eq!(chunk_shaping_text(text, &(0..2)), "a");
        // A chunk that does not end on a break is untouched.
        assert_eq!(chunk_shaping_text("abc", &(0..3)), "abc");
    }

    // ---- frozen_chunk_count ----------------------------------------------

    /// The freeze rule, at the boundary that decides it: a chunk is frozen
    /// only when it is *full*. A trailing chunk that merely happens to end on
    /// a newline (the block's text ends with one) is still going to grow, and
    /// freezing it would pin a stale layout for the rest of the stream.
    #[test]
    fn only_full_chunks_freeze() {
        // Two complete 2-line chunks, nothing left over.
        let text = "a\nb\nc\nd\n";
        let ranges = chunk_ranges(text, 2);
        assert_eq!(ranges.len(), 2);
        assert_eq!(frozen_chunk_count(text, &ranges, 2), 2);

        // Same text plus a partial line: the tail chunk is open.
        let text = "a\nb\nc\nd\ne";
        let ranges = chunk_ranges(text, 2);
        assert_eq!(ranges.len(), 3);
        assert_eq!(frozen_chunk_count(text, &ranges, 2), 2);

        // A tail that ends on a newline but isn't full yet — the case a
        // naive `ends_with('\n')` test would get wrong.
        let text = "a\nb\nc\n";
        let ranges = chunk_ranges(text, 2);
        assert_eq!(ranges.len(), 2);
        assert_eq!(frozen_chunk_count(text, &ranges, 2), 1);
    }

    #[test]
    fn a_single_open_chunk_freezes_nothing() {
        let text = "one\ntwo";
        assert_eq!(frozen_chunk_count(text, &chunk_ranges(text, 64), 64), 0);
        assert_eq!(frozen_chunk_count("", &chunk_ranges("", 64), 64), 0);
        assert_eq!(frozen_chunk_count("", &[], 64), 0);
    }

    /// The property the whole incremental-tail design rests on: appending to
    /// the end of a block never disturbs a frozen chunk's byte range. If this
    /// fails, reusing a frozen chunk's glyphs reuses them against different
    /// bytes.
    #[test]
    fn appending_never_moves_a_frozen_chunks_range() {
        let mut text = String::new();
        let mut frozen: Vec<Range<usize>> = Vec::new();
        for i in 0..40 {
            text.push_str(&format!("line {i}\n"));
            let ranges = chunk_ranges(&text, 4);
            let count = frozen_chunk_count(&text, &ranges, 4);
            for (j, range) in ranges[..count].iter().enumerate() {
                match frozen.get(j) {
                    Some(prev) => assert_eq!(prev, range, "frozen chunk {j} moved at append {i}"),
                    None => frozen.push(range.clone()),
                }
            }
            assert!(count <= ranges.len());
        }
        assert!(frozen.len() > 5, "test premise: chunks must have frozen");
    }

    // ---- slice_spans -----------------------------------------------------

    #[test]
    fn spans_inside_a_chunk_are_rebased_to_chunk_local_offsets() {
        let spans = [span(12, 16, BLUE)];
        assert_eq!(
            slice_spans(&spans, 10..20)
                .iter()
                .map(|s| (s.start, s.end))
                .collect::<Vec<_>>(),
            vec![(2, 6)],
        );
    }

    #[test]
    fn a_span_crossing_a_chunk_boundary_is_clamped_into_both_chunks() {
        // One span over [5, 15) against chunks [0,10) and [10,20).
        let spans = [span(5, 15, BLUE)];
        let first = slice_spans(&spans, 0..10);
        let second = slice_spans(&spans, 10..20);
        assert_eq!(
            first.iter().map(|s| (s.start, s.end)).collect::<Vec<_>>(),
            vec![(5, 10)],
        );
        assert_eq!(
            second.iter().map(|s| (s.start, s.end)).collect::<Vec<_>>(),
            vec![(0, 5)],
        );
    }

    #[test]
    fn spans_outside_the_chunk_are_dropped() {
        let spans = [span(0, 4, BLUE), span(30, 40, BLUE)];
        assert!(slice_spans(&spans, 10..20).is_empty());
    }

    #[test]
    fn a_span_touching_only_the_chunk_edge_is_dropped() {
        // [10, 10) after clamping — no glyph could carry it.
        let spans = [span(0, 10, BLUE), span(20, 25, BLUE)];
        assert!(slice_spans(&spans, 10..20).is_empty());
    }

    #[test]
    fn span_order_and_brushes_survive_slicing() {
        let spans = [span(0, 12, WHITE), span(12, 30, BLUE)];
        let sliced = slice_spans(&spans, 8..20);
        assert_eq!(
            sliced.iter().map(|s| (s.start, s.end)).collect::<Vec<_>>(),
            vec![(0, 4), (4, 12)],
        );
        assert_eq!(sliced[0].brush, brush(WHITE));
        assert_eq!(sliced[1].brush, brush(BLUE));
    }

    // ---- the equivalence property ----------------------------------------

    /// Stack per-chunk layouts the way `shape_cache` does: each chunk's
    /// glyphs are collected chunk-local (y from 0 at the chunk's top) and the
    /// running y is advanced by the chunk layout's own height. Returns the
    /// glyphs plus the stacked total height.
    fn stacked_chunk_glyphs(
        font: &VelloFont,
        text: &str,
        spans: &[SpanBrush],
        chunk_lines: usize,
        max_advance: Option<f32>,
    ) -> (Vec<PositionedGlyph>, f32) {
        let style = style();
        let fallback = brush(WHITE);
        let mut out = Vec::new();
        let mut y = 0.0_f32;
        for range in chunk_ranges(text, chunk_lines) {
            let chunk_text = chunk_shaping_text(text, &range);
            let chunk_spans = slice_spans(spans, range.clone());
            let layout = font.layout_spanned(
                chunk_text,
                &style,
                VelloTextAlign::Left,
                max_advance,
                &chunk_spans,
            );
            let (mut glyphs, _keys) =
                collect_msdf_glyphs_deferred(&layout, &chunk_spans, &fallback, (0.0, 0.0));
            for glyph in &mut glyphs {
                glyph.y += y;
            }
            out.extend(glyphs);
            y += layout.height();
        }
        (out, y)
    }

    /// Every glyph is the same glyph at the same x, and its y is within
    /// parley's per-layout baseline quantization (see the module docs).
    /// Returns the largest vertical disagreement, so callers can assert it
    /// doesn't grow with the chunk count.
    fn assert_same_placement(whole: &[PositionedGlyph], chunked: &[PositionedGlyph]) -> f32 {
        assert_eq!(whole.len(), chunked.len(), "glyph count differs");
        let mut worst_dy = 0.0_f32;
        for (i, (a, b)) in whole.iter().zip(chunked.iter()).enumerate() {
            assert_eq!(a.key, b.key, "glyph {i} is a different glyph");
            assert_eq!(a.x, b.x, "glyph {i} moved horizontally");
            let dy = (a.y - b.y).abs();
            assert!(
                dy < 1.0,
                "glyph {i} moved vertically by {dy}: {} vs {} — more than one \
                 baseline rounding, so the line grids genuinely disagree",
                a.y,
                b.y,
            );
            worst_dy = worst_dy.max(dy);
        }
        worst_dy
    }

    /// **The chunking contract.** Splitting a block on hard lines and
    /// stacking the per-chunk layouts reproduces the whole-block layout: same
    /// glyphs, same x, same total height to the bit, and y within one
    /// baseline rounding — including through soft wraps, which is the case a
    /// naive "one chunk per line" model would get wrong. If this fails,
    /// chunked shaping is not a valid substitution and the cache is drawing a
    /// different document than the geometry measured.
    #[test]
    fn chunked_and_whole_layouts_position_glyphs_identically() {
        let font = mono();
        let text = "the quick brown fox jumps over the lazy dog\n\
                    short\n\
                    \n\
                    a much longer line that will certainly need to wrap more than once at this width\n\
                    tail\n\
                    another line to push past a chunk boundary\n\
                    last";
        let max_advance = Some(160.0);

        let whole_layout =
            font.layout(text, &style(), VelloTextAlign::Left, max_advance);
        let (whole, _keys) =
            collect_msdf_glyphs_deferred(&whole_layout, &[], &brush(WHITE), (0.0, 0.0));

        // Two lines per chunk forces four boundaries over this text, one of
        // them right after the wrapped line.
        let (chunked, stacked_height) = stacked_chunk_glyphs(&font, text, &[], 2, max_advance);
        assert!(
            chunk_ranges(text, 2).len() > 1,
            "test premise: the text must actually split",
        );
        assert_same_placement(&whole, &chunked);
        assert_eq!(
            stacked_height,
            whole_layout.height(),
            "chunk heights must sum to the whole layout's height exactly — \
             this is what keeps every geometry offset chunking-independent",
        );
    }

    /// The bound that makes the sub-pixel disagreement acceptable: it is two
    /// roundings, not N. A document chunked into many pieces must not drift
    /// further from the whole-block layout than a two-chunk one does — if it
    /// did, chunk heights would be accumulating error and a long block would
    /// end up visibly off its own geometry.
    #[test]
    fn baseline_disagreement_does_not_accumulate_across_many_chunks() {
        let font = mono();
        let text = (0..200)
            .map(|i| format!("line {i} of a long block"))
            .collect::<Vec<_>>()
            .join("\n");
        let max_advance = Some(300.0);

        let whole_layout = font.layout(&text, &style(), VelloTextAlign::Left, max_advance);
        let (whole, _keys) =
            collect_msdf_glyphs_deferred(&whole_layout, &[], &brush(WHITE), (0.0, 0.0));

        let (chunked, stacked_height) = stacked_chunk_glyphs(&font, &text, &[], 4, max_advance);
        assert_eq!(chunk_ranges(&text, 4).len(), 50, "test premise: 50 chunks");

        let worst_dy = assert_same_placement(&whole, &chunked);
        assert!(
            worst_dy < 1.0,
            "50 chunks drifted {worst_dy}px — the error is accumulating",
        );
        // Not bit-exact at 50 chunks, but only because summing 50 `f32`
        // heights is: the residual is ~1e-3 px, three orders below the 0.01
        // threshold `ConversationGeometry::measure` treats as "moved".
        assert!(
            (stacked_height - whole_layout.height()).abs() < 0.01,
            "stacked {stacked_height} vs whole {}",
            whole_layout.height(),
        );
    }

    /// Same property with brush spans in play: `slice_spans` must rebase the
    /// spans so parley's run splitting lands in the same places, and coloring
    /// must survive a span that straddles a chunk boundary.
    #[test]
    fn chunked_spanned_layouts_match_whole_spanned_layouts() {
        let font = mono();
        let text = "alpha bravo charlie\ndelta echo foxtrot\ngolf hotel india\njuliet";
        // Straddles the first chunk boundary (byte 20 is in line 2).
        let spans = [span(6, 30, BLUE)];
        let max_advance = Some(200.0);

        let whole_layout = font.layout_spanned(
            text,
            &style(),
            VelloTextAlign::Left,
            max_advance,
            &spans,
        );
        let (whole, _keys) =
            collect_msdf_glyphs_deferred(&whole_layout, &spans, &brush(WHITE), (0.0, 0.0));

        let (chunked, _) = stacked_chunk_glyphs(&font, text, &spans, 2, max_advance);
        assert_same_placement(&whole, &chunked);
        assert!(
            whole.iter().any(|g| g.color == BLUE),
            "test premise: the span must color something",
        );
        for (a, b) in whole.iter().zip(chunked.iter()) {
            assert_eq!(a.color, b.color, "span coloring differs across chunking");
        }
    }

    /// A block with no hard breaks at all still round-trips: one chunk, one
    /// layout, identical placement — the common short-block case.
    #[test]
    fn single_chunk_text_is_untouched_by_chunking() {
        let font = mono();
        let text = "a wrapping paragraph with no hard breaks whatsoever in it";
        let max_advance = Some(120.0);
        let whole_layout = font.layout(text, &style(), VelloTextAlign::Left, max_advance);
        let (whole, _keys) =
            collect_msdf_glyphs_deferred(&whole_layout, &[], &brush(WHITE), (0.0, 0.0));
        let (chunked, _) = stacked_chunk_glyphs(&font, text, &[], CHUNK_LINES, max_advance);
        assert_same_placement(&whole, &chunked);
    }
}

