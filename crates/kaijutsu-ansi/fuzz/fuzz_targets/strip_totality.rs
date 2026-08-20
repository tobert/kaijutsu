//! Totality fuzz target: `strip` must never panic on arbitrary bytes, and
//! the spans it returns must always satisfy three invariants:
//!
//! - **Non-empty and well-formed**: `start < end`, strictly (an empty span
//!   describes nothing and `push_char` never emits one).
//! - **Sorted and disjoint**: each span starts no earlier than the previous
//!   one ended (`start >= prev_end`).
//! - **On UTF-8 char boundaries** within the returned text, at both
//!   endpoints.

#![no_main]

use kaijutsu_ansi::strip;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let (text, spans) = strip(data);

    let text_len = text.len() as u32;
    let mut prev_end: Option<u32> = None;

    for span in &spans {
        // Well-formed AND non-empty — an empty span is exactly the bug this
        // fuzzer should be well placed to find, so the strict form.
        assert!(span.start < span.end, "empty or inverted span: {span:?}");
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
