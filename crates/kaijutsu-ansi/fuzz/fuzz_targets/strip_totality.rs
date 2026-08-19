//! Totality fuzz target: `strip` must never panic on arbitrary bytes, and
//! the spans it returns must always satisfy the invariants the crate
//! documentation claims — sorted, disjoint (or at least non-overlapping in
//! a way that respects `end <= next.start`... actually we require strictly
//! adjacent-or-later, see below), and byte-offsets on UTF-8 char boundaries
//! within the returned text.

#![no_main]

use kaijutsu_ansi::strip;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (text, spans) = strip(data);

    let text_len = text.len() as u32;
    let mut prev_end: Option<u32> = None;

    for span in &spans {
        // Well-formed range.
        assert!(span.start <= span.end, "span start > end: {span:?}");
        // Never past the end of the text it's describing.
        assert!(span.end <= text_len, "span end {} > text len {} : {span:?}", span.end, text_len);

        // Both endpoints land on UTF-8 char boundaries in `text`.
        assert!(
            text.is_char_boundary(span.start as usize),
            "span start {} not a char boundary in {text:?}: {span:?}",
            span.start
        );
        assert!(
            text.is_char_boundary(span.end as usize),
            "span end {} not a char boundary in {text:?}: {span:?}",
            span.end
        );

        // Sorted and disjoint: each span starts no earlier than the
        // previous one ended. `push_char` only ever extends the last span
        // or appends a new one after it, so spans never overlap or go
        // backwards.
        if let Some(prev_end) = prev_end {
            assert!(
                span.start >= prev_end,
                "span out of order or overlapping: prev_end={prev_end}, span={span:?}"
            );
        }
        prev_end = Some(span.end);
    }
});
